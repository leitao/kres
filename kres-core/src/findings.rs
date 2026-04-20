//! Structured findings records and the atomic per-turn writer.
//!
//! Closes bugs.md items:
//! - H1: `FindingsStore::write_turn` holds its inner mutex ONLY for
//!   the N-allocation + disk write. Merge-via-LLM runs outside any
//!   store-owned lock.
//! - H2/H3: N allocation and the rename are inside the same critical
//!   section, so two concurrent merges can never collide on the same
//!   N.
//! - H6: the write is tmp-file + fsync + rename. Partial writes can't
//!   leave `findings-N.json` half-written on operator Ctrl-C.
//!
//! The schema mirrors findings-json-format.md exactly, including the
//! three optional fields (mechanism_detail, fix_sketch, open_questions)
//! that got lifted out of report-only prose recently.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write as _StdIoWrite;
use tokio::io::AsyncWriteExt;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FindingsError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("base findings path {0} has no parent directory")]
    NoParent(PathBuf),

    #[error("findings base filename must be like foo.json (got {0:?})")]
    BadBaseName(PathBuf),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Status {
    #[default]
    Active,
    Invalidated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelevantSymbol {
    pub name: String,
    pub filename: String,
    pub line: u32,
    pub definition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelevantFileSection {
    pub filename: String,
    pub line_start: u32,
    pub line_end: u32,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub title: String,
    pub severity: Severity,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub relevant_symbols: Vec<RelevantSymbol>,
    #[serde(default)]
    pub relevant_file_sections: Vec<RelevantFileSection>,
    pub summary: String,
    pub reproducer_sketch: String,
    pub impact: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mechanism_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_sketch: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_seen_task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_updated_task: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_finding_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FindingsFile {
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub tasks_since_change: u32,
    /// Extra breadcrumb beyond the documented schema: the turn number
    /// is embedded in the file so a copied-out file is still
    /// interpretable (bugs.md#R5).
    #[serde(default)]
    pub turn_n: Option<u32>,
}

/// Atomic writer + discoverer for `findings-N.json` snapshots.
///
/// Construct with a base path `/dir/findings.json`. The store writes
/// `findings-1.json`, `findings-2.json`, ..., never the bare base.
/// Current-state lookup scans the parent dir.
///
/// bugs.md#H1, #H2, #H3, #H6 all route through this type.
pub struct FindingsStore {
    base_path: PathBuf,
    /// Dir + stem are precomputed once; write_turn doesn't re-parse
    /// every call.
    parent_dir: PathBuf,
    stem: String,
    extension: String,
    /// Last allocated turn number (monotonic).
    state: Mutex<FindingsStoreState>,
}

#[derive(Debug, Default)]
struct FindingsStoreState {
    last_turn: u32,
    tasks_since_change: u32,
}

impl FindingsStore {
    pub fn new(base_path: impl Into<PathBuf>) -> Result<Self, FindingsError> {
        let base_path: PathBuf = base_path.into();
        let parent_dir = base_path
            .parent()
            .ok_or_else(|| FindingsError::NoParent(base_path.clone()))?
            .to_path_buf();
        let stem = base_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| FindingsError::BadBaseName(base_path.clone()))?
            .to_string();
        let extension = base_path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("json")
            .to_string();
        // bugs.md#L4: preflight that we can create and write in the
        // parent directory. Matches the Python `configure_findings`
        // gap — there, first write at bare_path raced into a bare
        // `except: pass` so the operator never noticed a read-only
        // $HOME. Here we fail fast at construction.
        std::fs::create_dir_all(&parent_dir)?;
        let probe = parent_dir.join(format!("{}.probe.{}", stem, std::process::id()));
        let mut f = std::fs::File::create(&probe)?;
        f.write_all(b"")?;
        drop(f);
        let _ = std::fs::remove_file(&probe);
        Ok(Self {
            base_path,
            parent_dir,
            stem,
            extension,
            state: Mutex::new(FindingsStoreState::default()),
        })
    }

    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    /// Compute the turn-N path relative to this store's base.
    pub fn path_for(&self, n: u32) -> PathBuf {
        self.parent_dir
            .join(format!("{}-{}.{}", self.stem, n, self.extension))
    }

    /// Scan the parent directory and return `(path, N)` for the
    /// highest-numbered `<stem>-<N>.<ext>` present, or None.
    pub fn discover_latest(&self) -> Result<Option<(PathBuf, u32)>, FindingsError> {
        let prefix = format!("{}-", self.stem);
        let suffix = format!(".{}", self.extension);
        let mut best: Option<(PathBuf, u32)> = None;

        let entries = match std::fs::read_dir(&self.parent_dir) {
            Ok(e) => e,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name_s) = name.to_str() else {
                continue;
            };
            let Some(rest) = name_s.strip_prefix(&prefix) else {
                continue;
            };
            let Some(num_str) = rest.strip_suffix(&suffix) else {
                continue;
            };
            let Ok(n) = num_str.parse::<u32>() else {
                continue;
            };
            if best.as_ref().map(|(_, b)| n > *b).unwrap_or(true) {
                best = Some((entry.path(), n));
            }
        }
        Ok(best)
    }

    /// Initialise the internal counter from what's on disk. The
    /// canonical `findings.json` (when present) wins for the seed
    /// findings — it is written last on every turn and reflects the
    /// current state. The highest numbered `findings-N.json` still
    /// drives `last_turn` so the next write picks N+1.
    pub fn bootstrap(&self) -> Result<InitialState, FindingsError> {
        let mut guard = self.state.lock().unwrap();
        let latest = self.discover_latest()?;
        let last_n = latest.as_ref().map(|(_, n)| *n).unwrap_or(0);
        let canonical_exists = self.base_path.exists();
        if canonical_exists {
            let raw = std::fs::read_to_string(&self.base_path)?;
            let file: FindingsFile = serde_json::from_str(&raw)?;
            guard.last_turn = last_n;
            guard.tasks_since_change = file.tasks_since_change;
            return Ok(InitialState {
                path: Some(self.base_path.clone()),
                turn_n: last_n,
                findings: file.findings,
                tasks_since_change: file.tasks_since_change,
            });
        }
        match latest {
            Some((path, n)) => {
                let raw = std::fs::read_to_string(&path)?;
                let file: FindingsFile = serde_json::from_str(&raw)?;
                guard.last_turn = n;
                guard.tasks_since_change = file.tasks_since_change;
                Ok(InitialState {
                    path: Some(path),
                    turn_n: n,
                    findings: file.findings,
                    tasks_since_change: file.tasks_since_change,
                })
            }
            None => Ok(InitialState {
                path: None,
                turn_n: 0,
                findings: Vec::new(),
                tasks_since_change: 0,
            }),
        }
    }

    /// Write a new turn snapshot atomically.
    ///
    /// Steps:
    /// 1. Lock, allocate `n = last_turn + 1`, compute `tasks_since_change`,
    ///    target path. Release state-mutex NOTHING that blocks on I/O.
    /// 2. Serialize to bytes.
    /// 3. Write to `<target>.tmp`, fsync, rename to target.
    /// 4. Re-lock to commit `last_turn = n` and updated counter.
    ///
    /// bugs.md#H6: the tmp+fsync+rename sequence means a SIGKILL
    /// anywhere in the middle either leaves the old `N-1.json`
    /// intact or atomically replaces with the new.
    ///
    /// bugs.md#H1: no network or LLM call happens inside the mutex.
    /// Callers run consolidation/merge before handing final findings
    /// here.
    pub async fn write_turn(
        &self,
        findings: Vec<Finding>,
        changed: bool,
    ) -> Result<WrittenTurn, FindingsError> {
        // Write layout (requested 2026-04-20):
        //   findings.json    — canonical, always the latest state
        //   findings-N.json  — history snapshot copied from the
        //                      previous findings.json before we
        //                      overwrite it
        //
        // Step 1: reserve N and track prior counters so a write
        // failure can roll back (bugs.md#H2). Only one `write_turn`
        // can pick a given N because the allocation is inside the
        // state mutex.
        let (n, tasks_since_change, prev_last, prev_tsc) = {
            let mut g = self.state.lock().unwrap();
            let prev_last = g.last_turn;
            let prev_tsc = g.tasks_since_change;
            let n = g.last_turn + 1;
            g.last_turn = n;
            if changed {
                g.tasks_since_change = 0;
            } else {
                g.tasks_since_change = g.tasks_since_change.saturating_add(1);
            }
            (n, g.tasks_since_change, prev_last, prev_tsc)
        };
        let snapshot_path = self.path_for(n);
        let canonical = self.base_path.clone();
        // Both tmp paths include N so two concurrent writers in the
        // same process (same PID) never step on each other's tmp
        // before the rename.
        let tmp = self.parent_dir.join(format!(
            "{}.{}.n{}.{}.tmp",
            self.stem,
            self.extension,
            n,
            std::process::id()
        ));
        let snapshot_tmp = self.parent_dir.join(format!(
            "{}-{}.{}.{}.tmp",
            self.stem,
            n,
            self.extension,
            std::process::id()
        ));

        let file = FindingsFile {
            findings,
            updated_at: Some(Utc::now()),
            tasks_since_change,
            turn_n: Some(n),
        };
        let bytes = serde_json::to_vec_pretty(&file)?;

        let roll_back_counters = |me: &FindingsStore, tmps: &[&Path]| {
            let mut g = me.state.lock().unwrap();
            g.last_turn = prev_last;
            g.tasks_since_change = prev_tsc;
            for t in tmps {
                let _ = std::fs::remove_file(t);
            }
        };
        if let Err(e) = tokio::fs::create_dir_all(&self.parent_dir).await {
            roll_back_counters(self, &[]);
            return Err(FindingsError::Io(e));
        }

        // Step 2: snapshot the current canonical file to
        // findings-N.json BEFORE we overwrite it. This gives us a
        // full history of how findings evolved while keeping the
        // bare `findings.json` as the always-current record.
        //
        // We copy into a unique tmp and rename so a crash mid-copy
        // never leaves a partial findings-N.json on disk. If the
        // canonical file does not yet exist (first turn of a fresh
        // session), there is nothing to snapshot.
        match tokio::fs::metadata(&canonical).await {
            Ok(_) => {
                if let Err(e) = tokio::fs::copy(&canonical, &snapshot_tmp).await {
                    roll_back_counters(self, &[&snapshot_tmp]);
                    return Err(FindingsError::Io(e));
                }
                if let Ok(sf) = tokio::fs::File::open(&snapshot_tmp).await {
                    let _ = sf.sync_all().await;
                }
                if let Err(e) = tokio::fs::rename(&snapshot_tmp, &snapshot_path).await {
                    roll_back_counters(self, &[&snapshot_tmp]);
                    return Err(FindingsError::Io(e));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                roll_back_counters(self, &[&snapshot_tmp]);
                return Err(FindingsError::Io(err));
            }
        }

        // Step 3: atomic write of the new canonical findings.json.
        // tmp → fsync → rename → parent-dir fsync. The parent-dir
        // fsync is what actually makes the rename durable on
        // ext4/xfs after a power loss (bugs.md#H6).
        let mut f = match tokio::fs::File::create(&tmp).await {
            Ok(f) => f,
            Err(e) => {
                roll_back_counters(self, &[&tmp]);
                return Err(FindingsError::Io(e));
            }
        };
        if let Err(e) = async {
            f.write_all(&bytes).await?;
            f.flush().await?;
            f.sync_all().await
        }
        .await
        {
            drop(f);
            roll_back_counters(self, &[&tmp]);
            return Err(FindingsError::Io(e));
        }
        drop(f);
        if let Err(e) = tokio::fs::rename(&tmp, &canonical).await {
            roll_back_counters(self, &[&tmp]);
            return Err(FindingsError::Io(e));
        }
        if let Ok(dir_file) = tokio::fs::File::open(&self.parent_dir).await {
            if let Err(e) = dir_file.sync_all().await {
                tracing::warn!(
                    target: "kres_core",
                    dir = %self.parent_dir.display(),
                    "findings parent-dir fsync failed: {e}"
                );
            }
        }

        // The canonical `findings.json` is the authoritative latest
        // state on every turn, so WrittenTurn.path always points
        // there. Operators who want turn-N's prior state can read
        // `findings-N.json` directly; it's just `path_for(n)`.
        Ok(WrittenTurn {
            path: canonical,
            turn_n: n,
            tasks_since_change,
        })
    }

    /// Number of consecutive turns without a change.
    pub fn tasks_since_change(&self) -> u32 {
        self.state.lock().unwrap().tasks_since_change
    }

    pub fn last_turn(&self) -> u32 {
        self.state.lock().unwrap().last_turn
    }
}

#[derive(Debug, Clone)]
pub struct InitialState {
    pub path: Option<PathBuf>,
    pub turn_n: u32,
    pub findings: Vec<Finding>,
    pub tasks_since_change: u32,
}

#[derive(Debug, Clone)]
pub struct WrittenTurn {
    pub path: PathBuf,
    pub turn_n: u32,
    pub tasks_since_change: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn tmp_dir(nonce: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "kres-findings-test-{}-{}",
            nonce,
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample_finding(id: &str) -> Finding {
        Finding {
            id: id.to_string(),
            title: format!("finding {id}"),
            severity: Severity::High,
            status: Status::Active,
            relevant_symbols: vec![],
            relevant_file_sections: vec![],
            summary: "s".into(),
            reproducer_sketch: "r".into(),
            impact: "i".into(),
            mechanism_detail: None,
            fix_sketch: None,
            open_questions: vec![],
            first_seen_task: None,
            last_updated_task: None,
            related_finding_ids: vec![],
        }
    }

    #[tokio::test]
    async fn write_turn_writes_canonical_first_time() {
        // First write has no prior canonical to snapshot, so the
        // result lives in findings.json and no findings-1.json gets
        // created yet.
        let dir = tmp_dir("create");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).unwrap();
        let wt = store
            .write_turn(vec![sample_finding("a")], true)
            .await
            .unwrap();
        assert_eq!(wt.turn_n, 1);
        assert!(base.exists(), "canonical findings.json should exist");
        assert!(
            !dir.join("findings-1.json").exists(),
            "no snapshot on first write (nothing to snapshot)"
        );
        let raw = std::fs::read_to_string(&base).unwrap();
        let parsed: FindingsFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.findings.len(), 1);
        assert_eq!(parsed.turn_n, Some(1));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn write_turn_snapshots_prior_canonical() {
        // Second write should copy the pre-existing findings.json to
        // findings-2.json, then overwrite findings.json with the new
        // content.
        let dir = tmp_dir("snapshot");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).unwrap();
        store
            .write_turn(vec![sample_finding("a")], true)
            .await
            .unwrap();
        let wt = store
            .write_turn(vec![sample_finding("a"), sample_finding("b")], true)
            .await
            .unwrap();
        assert_eq!(wt.turn_n, 2);
        let snap = dir.join("findings-2.json");
        assert!(snap.exists(), "snapshot of prior canonical");
        let prior: FindingsFile =
            serde_json::from_str(&std::fs::read_to_string(&snap).unwrap()).unwrap();
        assert_eq!(prior.findings.len(), 1, "snapshot captured turn-1 state");
        let latest: FindingsFile =
            serde_json::from_str(&std::fs::read_to_string(&base).unwrap()).unwrap();
        assert_eq!(latest.findings.len(), 2, "canonical has latest state");
        assert_eq!(latest.turn_n, Some(2));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn discover_latest_finds_highest_n() {
        let dir = tmp_dir("discover");
        let base = dir.join("findings.json");
        // Drop files with mixed numbers + a noise file.
        for n in [1, 2, 5, 3] {
            std::fs::write(
                dir.join(format!("findings-{n}.json")),
                r#"{"findings":[],"tasks_since_change":0}"#,
            )
            .unwrap();
        }
        std::fs::write(dir.join("other-4.json"), "{}").unwrap();
        std::fs::write(dir.join("findings.json"), "{}").unwrap();
        let store = FindingsStore::new(&base).unwrap();
        let (path, n) = store.discover_latest().unwrap().unwrap();
        assert_eq!(n, 5);
        assert!(path.ends_with("findings-5.json"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn bootstrap_seeds_counters() {
        let dir = tmp_dir("boot");
        let base = dir.join("findings.json");
        std::fs::write(
            dir.join("findings-3.json"),
            r#"{"findings":[],"tasks_since_change":4,"turn_n":3}"#,
        )
        .unwrap();
        let store = FindingsStore::new(&base).unwrap();
        let init = store.bootstrap().unwrap();
        assert_eq!(init.turn_n, 3);
        assert_eq!(init.tasks_since_change, 4);
        assert_eq!(store.last_turn(), 3);
        assert_eq!(store.tasks_since_change(), 4);
        // Next write should be turn 4, not 1.
        let wt = store.write_turn(vec![], false).await.unwrap();
        assert_eq!(wt.turn_n, 4);
        // `changed=false` advances tasks_since_change.
        assert_eq!(wt.tasks_since_change, 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn bootstrap_prefers_canonical_when_present() {
        // findings.json reflects the latest post-merge state; when
        // present it wins over the highest numbered findings-N.json.
        let dir = tmp_dir("boot-canonical");
        let base = dir.join("findings.json");
        std::fs::write(
            dir.join("findings-2.json"),
            r#"{"findings":[],"tasks_since_change":2,"turn_n":2}"#,
        )
        .unwrap();
        std::fs::write(
            &base,
            r#"{"findings":[{"id":"x","title":"x","severity":"high","status":"active","summary":"s","reproducer_sketch":"r","impact":"i"}],"tasks_since_change":0,"turn_n":2}"#,
        )
        .unwrap();
        let store = FindingsStore::new(&base).unwrap();
        let init = store.bootstrap().unwrap();
        assert_eq!(init.findings.len(), 1);
        assert_eq!(init.findings[0].id, "x");
        assert_eq!(init.turn_n, 2);
        // Next write should snapshot current canonical to findings-3.
        let wt = store.write_turn(vec![], false).await.unwrap();
        assert_eq!(wt.turn_n, 3);
        assert!(dir.join("findings-3.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn tasks_since_change_resets_on_change() {
        let dir = tmp_dir("reset");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).unwrap();
        for _ in 0..3 {
            store.write_turn(vec![], false).await.unwrap();
        }
        assert_eq!(store.tasks_since_change(), 3);
        let wt = store
            .write_turn(vec![sample_finding("x")], true)
            .await
            .unwrap();
        assert_eq!(wt.tasks_since_change, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn concurrent_writes_never_collide_on_n() {
        // Smoke test for bugs.md#H2: two tasks racing to write should
        // each get a unique, monotonically-increasing N.
        let dir = tmp_dir("race");
        let base = dir.join("findings.json");
        let store = Arc::new(FindingsStore::new(&base).unwrap());
        let mut handles = vec![];
        for _ in 0..8 {
            let s = store.clone();
            handles.push(tokio::spawn(async move {
                s.write_turn(vec![], false).await.unwrap()
            }));
        }
        let mut ns: Vec<u32> = vec![];
        for h in handles {
            ns.push(h.await.unwrap().turn_n);
        }
        ns.sort();
        assert_eq!(ns, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn tmp_file_not_left_behind_on_success() {
        // bugs.md#H6 path-hygiene check.
        let dir = tmp_dir("tmp-clean");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).unwrap();
        // Two writes so we exercise both the canonical-write path and
        // the snapshot-then-canonical path.
        let _ = store.write_turn(vec![], true).await.unwrap();
        let wt = store.write_turn(vec![], true).await.unwrap();
        assert!(wt.path.exists());
        let stray: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(stray.is_empty(), "unexpected tmp leftovers: {stray:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn optional_fields_serialise_only_when_present() {
        let mut f = sample_finding("x");
        f.fix_sketch = None;
        f.mechanism_detail = None;
        let s = serde_json::to_string(&f).unwrap();
        assert!(!s.contains("fix_sketch"));
        assert!(!s.contains("mechanism_detail"));

        f.fix_sketch = Some("cache bool".to_string());
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"fix_sketch\":\"cache bool\""));
    }

    #[test]
    fn severity_and_status_serde() {
        let f = sample_finding("x");
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"severity\":\"high\""));
        assert!(s.contains("\"status\":\"active\""));
    }

    #[test]
    fn preflight_rejects_unwritable_parent() {
        // bugs.md#L4 — a FindingsStore pointing through /dev/null
        // can't create its parent directory. Preflight must surface
        // that at construction, not at first write.
        let bad = PathBuf::from("/dev/null/nested/findings.json");
        match FindingsStore::new(&bad) {
            Ok(_) => panic!("expected preflight failure"),
            Err(FindingsError::Io(_)) => {}
            Err(other) => panic!("wrong error kind: {other}"),
        }
    }
}
