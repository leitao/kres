//! Todo-agent: maintains the todo list based on task output.
//!
//! Port of
//!
//! After each task completes, the caller feeds this module:
//!   - the prompt that drove the task (completed_query)
//!   - the task's analysis text (analysis_summary)
//!   - the followups the slow agent produced (new_followups)
//!   - the current todo list
//!   - optional session-wide lenses
//!
//! The module packages that into a JSON request (with
//! `analysis_citations`, REPRIORITIZE + DEDUP + COVERAGE instructions)
//! and sends it through a dedicated todo-agent inference. The response
//! is parsed back into a new todo list with:
//!   - done items the agent dropped preserved (coverage signal)
//!   - missing coverage on done items carried forward
//!   - a programmatic dedup backstop for pending items
//!   - a pending cap of 20 (done items don't count)
//!
//! On any failure we fall back to a token-overlap dedup that merges
//! the new followups into the existing list — the todo list must
//! never regress because of a flaky API call.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use kres_core::lens::LensSpec;
use kres_core::log::{LoggedUsage, TurnLogger};
use kres_core::todo::{TodoItem, TodoStatus};
use kres_llm::{client::Client, config::CallConfig, request::Message, Model};

use crate::error::AgentError;

pub const TODO_INSTRUCTIONS: &str = include_str!("prompts/todo.txt");

/// Config bundle for the todo agent.
#[derive(Clone)]
pub struct TodoClient {
    pub client: Arc<Client>,
    pub model: Model,
    pub system: Option<String>,
    pub max_tokens: u32,
    pub max_input_tokens: Option<u32>,
}

/// Parsed response shape from the todo agent.
#[derive(Debug, Deserialize)]
struct TodoUpdateResponse {
    #[serde(default)]
    todo: Value,
}

/// Run the todo agent. Returns an updated list. Matches
pub async fn update_todo_via_agent(
    tc: &TodoClient,
    completed_query: &str,
    analysis_summary: &str,
    new_followups: &[Value],
    current_todo: &[TodoItem],
    lenses: &[LensSpec],
) -> Result<Vec<TodoItem>, AgentError> {
    update_todo_via_agent_with_logger(
        tc,
        completed_query,
        analysis_summary,
        new_followups,
        current_todo,
        lenses,
        None,
    )
    .await
}

/// Same as `update_todo_via_agent` but also logs the user+assistant
/// turns to the provided TurnLogger's `main.jsonl`
#[allow(clippy::too_many_arguments)]
pub async fn update_todo_via_agent_with_logger(
    tc: &TodoClient,
    completed_query: &str,
    analysis_summary: &str,
    new_followups: &[Value],
    current_todo: &[TodoItem],
    lenses: &[LensSpec],
    logger: Option<Arc<TurnLogger>>,
) -> Result<Vec<TodoItem>, AgentError> {
    // --- Prepare inputs ------------------------------------------------
    let mut todo_list = current_todo.to_vec();
    assign_ids(&mut todo_list);
    let current_payload: Vec<Value> = todo_list.iter().map(todo_to_payload).collect();

    let lens_payload: Vec<Value> = lenses
        .iter()
        .map(|l| {
            json!({
                "type": l.kind,
                "name": l.name,
                "reason": l.reason,
            })
        })
        .collect();

    // Cap analysis_summary at 15k chars.
    let analysis_capped: String = analysis_summary.chars().take(15_000).collect();
    let citations = extract_citations(analysis_summary);

    let mut request = serde_json::Map::new();
    request.insert("task".into(), json!("update_todo"));
    request.insert("completed_query".into(), json!(completed_query));
    request.insert("analysis_summary".into(), json!(analysis_capped));
    request.insert("analysis_citations".into(), json!(citations));
    request.insert("new_followups".into(), json!(new_followups));
    request.insert("current_todo".into(), json!(current_payload));
    if !lens_payload.is_empty() {
        request.insert("lenses".into(), json!(lens_payload));
    }
    request.insert(
        "instructions".into(),
        json!(build_instructions(!lens_payload.is_empty())),
    );
    let request_text = serde_json::to_string_pretty(&Value::Object(request))?;

    // --- Send inference ------------------------------------------------
    let mut cfg = CallConfig::defaults_for(tc.model.clone())
        .with_max_tokens(tc.max_tokens)
        .with_stream_label("todo update");
    if let Some(s) = &tc.system {
        cfg = cfg.with_system(s.clone());
    }
    if let Some(n) = tc.max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }
    let messages = vec![Message {
        role: "user".into(),
        content: request_text.clone(),
        cache: true,
        cached_prefix: None,
    }];
    if let Some(lg) = &logger {
        lg.log_main("user", &request_text, None, None);
    }

    let resp_result = tc.client.messages_streaming(&cfg, &messages).await;
    let resp = match resp_result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "kres_agents", "todo agent call failed: {e}; falling back");
            return Ok(fallback_dedup(&todo_list, new_followups));
        }
    };
    let text = extract_text(&resp);
    if let Some(lg) = &logger {
        lg.log_main(
            "assistant",
            &text,
            Some(LoggedUsage {
                input: resp.usage.input_tokens,
                output: resp.usage.output_tokens,
                cache_creation: resp.usage.cache_creation_input_tokens,
                cache_read: resp.usage.cache_read_input_tokens,
            }),
            None,
        );
    }

    // --- Parse response ------------------------------------------------
    let parsed: Vec<TodoItem> = match parse_todo_response(&text) {
        Some(v) => v,
        None => {
            tracing::warn!(target: "kres_agents", "todo agent returned no parseable list; falling back");
            return Ok(fallback_dedup(&todo_list, new_followups));
        }
    };

    // --- Reconcile with existing done items ---------------------------
    let (done_from_agent, pending_from_agent): (Vec<TodoItem>, Vec<TodoItem>) = parsed
        .into_iter()
        .partition(|t| t.status == TodoStatus::Done);
    let original_done: HashMap<String, TodoItem> = todo_list
        .iter()
        .filter(|t| t.status == TodoStatus::Done)
        .filter(|t| !t.id.is_empty())
        .map(|t| (t.id.clone(), t.clone()))
        .collect();
    let agent_done_ids: HashSet<String> = done_from_agent
        .iter()
        .filter(|t| !t.id.is_empty())
        .map(|t| t.id.clone())
        .collect();
    let preserved: Vec<TodoItem> = original_done
        .iter()
        .filter(|(id, _)| !agent_done_ids.contains(*id))
        .map(|(_, t)| t.clone())
        .collect();

    // Carry forward prior coverage when the agent dropped it.
    let mut done_final = done_from_agent;
    for d in &mut done_final {
        if d.coverage.is_empty() {
            if let Some(orig) = original_done.get(&d.id) {
                if !orig.coverage.is_empty() {
                    d.coverage = orig.coverage.clone();
                }
            }
        }
    }

    // --- Programmatic dedup backstop for pending items ----------------
    let mut ref_token_sets: Vec<(String, HashSet<String>)> = Vec::new();
    for d in done_final.iter().chain(preserved.iter()) {
        let bag = format!("{} {} {}", d.name, d.reason, d.coverage);
        let toks = dedup_tokens(&bag);
        if !toks.is_empty() {
            ref_token_sets.push((d.name.clone(), toks));
        }
    }
    let mut filtered_pending: Vec<TodoItem> = Vec::new();
    let mut dropped: Vec<(String, String)> = Vec::new();
    for p in pending_from_agent.into_iter() {
        let bag = format!("{} {}", p.name, p.reason);
        let ptoks = dedup_tokens(&bag);
        if ptoks.is_empty() {
            filtered_pending.push(p);
            continue;
        }
        let mut dup = false;
        for (dname, dtoks) in &ref_token_sets {
            let overlap = ptoks.intersection(dtoks).count();
            let denom = ptoks.len().min(dtoks.len());
            if denom > 0 && (overlap as f64) / (denom as f64) >= 0.7 {
                dup = true;
                dropped.push((p.name.clone(), dname.clone()));
                break;
            }
        }
        if !dup {
            ref_token_sets.push((p.name.clone(), ptoks));
            filtered_pending.push(p);
        }
    }

    if !dropped.is_empty() {
        tracing::info!(
            target: "kres_agents",
            "todo agent dedup dropped {} pending item(s): {}",
            dropped.len(),
            dropped
                .iter()
                .take(3)
                .map(|(p, d)| format!("{}≈{}", truncate(p, 40), truncate(d, 40)))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    // --- Apply pending cap (done items don't count) -------------------
    const PENDING_CAP: usize = 20;
    if filtered_pending.len() > PENDING_CAP {
        filtered_pending.truncate(PENDING_CAP);
    }

    // Order: done-from-agent, preserved-done, filtered-pending
    let mut result =
        Vec::with_capacity(done_final.len() + preserved.len() + filtered_pending.len());
    result.extend(done_final);
    result.extend(preserved);
    result.extend(filtered_pending);
    Ok(result)
}

fn todo_to_payload(t: &TodoItem) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), json!(t.id));
    obj.insert("type".into(), json!(t.kind));
    obj.insert("name".into(), json!(t.name));
    obj.insert("reason".into(), json!(t.reason));
    obj.insert(
        "status".into(),
        json!(match t.status {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "pending",
            TodoStatus::Blocked => "pending",
            TodoStatus::Done => "done",
            TodoStatus::Skipped => "done",
        }),
    );
    obj.insert("depends_on".into(), json!(t.depends_on));
    if !t.coverage.is_empty() {
        obj.insert("coverage".into(), json!(t.coverage));
    }
    Value::Object(obj)
}

/// Assign a short unique id to every item that doesn't have one.
fn assign_ids(list: &mut [TodoItem]) {
    let mut seen: HashSet<String> = HashSet::new();
    for t in list.iter_mut() {
        if !t.id.is_empty() && !seen.contains(&t.id) {
            seen.insert(t.id.clone());
            continue;
        }
        let base: String = t.name.chars().take(40).collect();
        let mut id = base.clone();
        let mut counter = 2u32;
        while seen.contains(&id) {
            let short: String = base.chars().take(37).collect();
            id = format!("{short}_{counter}");
            counter += 1;
        }
        seen.insert(id.clone());
        t.id = id;
    }
}

/// Extract `path:line[-line]` citations from analysis text. Returns a
/// sorted-deduped list, capped at 200 entries.
pub fn extract_citations(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = std::collections::BTreeSet::new();
    // Same regex as : file extension gate + line-or-range capture.
    // Rust's regex crate supports lookbehind-free constructs; use a
    // hand-coded scan to avoid pulling in a new dependency.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Scan for a candidate that ends in one of the allowed file
        // extensions then `:digits[-digits]`. Walk forward char by
        // char looking for a `.` followed by an extension token.
        if bytes[i] == b'.' {
            // match the extension at bytes[i..]
            for ext in &[
                ".c", ".h", ".bpf.c", ".go", ".py", ".rs", ".S", ".s", ".md", ".sh",
            ] {
                let e = ext.as_bytes();
                if i + e.len() <= bytes.len() && &bytes[i..i + e.len()] == e {
                    let mut end_ext = i + e.len();
                    // Must be followed by `:digits`
                    if end_ext < bytes.len() && bytes[end_ext] == b':' {
                        let digits_start = end_ext + 1;
                        let mut j = digits_start;
                        while j < bytes.len() && bytes[j].is_ascii_digit() {
                            j += 1;
                        }
                        if j == digits_start {
                            continue;
                        }
                        let mut range_end = j;
                        if j < bytes.len() && bytes[j] == b'-' {
                            let mut k = j + 1;
                            while k < bytes.len() && bytes[k].is_ascii_digit() {
                                k += 1;
                            }
                            if k > j + 1 {
                                range_end = k;
                            }
                        }
                        // Walk backwards to find the start of the path
                        // (allow `[\w./+-]*`).
                        let mut p = i;
                        while p > 0 {
                            let c = bytes[p - 1] as char;
                            if c.is_ascii_alphanumeric()
                                || c == '.'
                                || c == '/'
                                || c == '_'
                                || c == '+'
                                || c == '-'
                            {
                                p -= 1;
                            } else {
                                break;
                            }
                        }
                        let cite = std::str::from_utf8(&bytes[p..range_end])
                            .ok()
                            .map(|s| s.to_string());
                        if let Some(c) = cite {
                            // Reject bare trailing ".ext" with no path
                            // (accidentally match "hit .c:123"). The
                            // regex prefix `[\w./][\w./+-]*` lets
                            // any of word/./ start the path — so empty
                            // before `.ext` is OK as long as `.ext`
                            // itself sits after `[\w./]` — skip if p == i
                            // (zero-length path component) and the
                            // char before is whitespace (a false
                            // match). Simpler: require path length
                            // >= ext length + 2 (at least `x.c` form).
                            if c.len() >= e.len() + 2 {
                                out.insert(c);
                            }
                        }
                        end_ext = range_end;
                        i = end_ext;
                        break;
                    }
                    let _ = end_ext;
                }
            }
        }
        i += 1;
    }
    out.into_iter().take(200).collect()
}

/// DEDUP_STOP_TOKENS — common words we don't want skewing the token
/// overlap when deduping todo items.
const DEDUP_STOP_TOKENS: &[&str] = &[
    "this", "from", "into", "when", "what", "which", "same", "each", "also", "then", "than",
    "there", "their", "before", "after", "entry", "entries", "show", "dump", "print", "name",
    "names", "path", "paths", "point", "points", "case", "cases", "call", "calls", "data", "head",
    "tail",
];

/// Extract tokens useful for near-duplicate detection of todo items.
/// Lowercased file paths, section refs (§3b), and C-identifier-like
/// substrings of length >= 5.
pub fn dedup_tokens(s: &str) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    if s.is_empty() {
        return out;
    }
    let lower = s.to_lowercase();
    // Pass 1: file-path tokens. Lean scan mirroring
    for ext in &[
        ".c", ".h", ".bpf.c", ".go", ".py", ".rs", ".s", ".md", ".sh",
    ] {
        let mut start = 0;
        while let Some(off) = lower[start..].find(ext) {
            let abs = start + off;
            let after = abs + ext.len();
            // Next char must not be alpha-numeric (avoid "foo.cpp" etc).
            let after_ok = lower
                .as_bytes()
                .get(after)
                .map(|c| !(*c as char).is_ascii_alphanumeric())
                .unwrap_or(true);
            if after_ok {
                // Walk back over path-allowed characters.
                let mut p = abs;
                while p > 0 {
                    let c = lower.as_bytes()[p - 1] as char;
                    if c.is_ascii_alphanumeric()
                        || c == '.'
                        || c == '/'
                        || c == '_'
                        || c == '+'
                        || c == '-'
                    {
                        p -= 1;
                    } else {
                        break;
                    }
                }
                if after > p + ext.len() {
                    out.insert(lower[p..after].to_string());
                }
            }
            start = after;
        }
    }
    // Pass 2: section refs like "§3b".
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '§' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                let mut end = j;
                if end < chars.len() && chars[end].is_ascii_lowercase() {
                    end += 1;
                }
                let tok: String = chars[i..end].iter().collect();
                out.insert(tok.to_lowercase());
                i = end;
                continue;
            }
        }
        i += 1;
    }
    // Pass 3: identifiers of length >= 5 that aren't stop words.
    let mut tok = String::new();
    let bytes = lower.as_bytes();
    for &b in bytes {
        let c = b as char;
        if c.is_ascii_alphanumeric() || c == '_' {
            tok.push(c);
        } else {
            flush_tok(&mut tok, &mut out);
        }
    }
    flush_tok(&mut tok, &mut out);
    out
}

fn flush_tok(tok: &mut String, out: &mut HashSet<String>) {
    if tok.len() >= 5
        && !tok
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(true)
        && !DEDUP_STOP_TOKENS.contains(&tok.as_str())
    {
        out.insert(std::mem::take(tok));
    }
    tok.clear();
}

/// Fallback path: token-overlap dedup of new_followups into the
/// existing todo list when the API call fails.
fn fallback_dedup(existing: &[TodoItem], new_followups: &[Value]) -> Vec<TodoItem> {
    let mut out = existing.to_vec();
    let mut existing_tokens: Vec<HashSet<String>> = out
        .iter()
        .map(|t| dedup_tokens(&format!("{} {} {}", t.name, t.reason, t.coverage)))
        .filter(|s| !s.is_empty())
        .collect();
    for fu in new_followups {
        let name = fu.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let reason = fu.get("reason").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let fu_toks = dedup_tokens(&format!("{name} {reason}"));
        if fu_toks.is_empty() {
            if let Ok(item) = followup_to_todo(fu) {
                out.push(item);
            }
            continue;
        }
        let mut dup = false;
        for etoks in &existing_tokens {
            let overlap = fu_toks.intersection(etoks).count();
            let denom = fu_toks.len().min(etoks.len());
            if denom > 0 && (overlap as f64) / (denom as f64) >= 0.7 {
                dup = true;
                break;
            }
        }
        if !dup {
            if let Ok(item) = followup_to_todo(fu) {
                existing_tokens.push(fu_toks);
                out.push(item);
            }
        }
    }
    out
}

fn followup_to_todo(fu: &Value) -> Result<TodoItem, serde_json::Error> {
    // Followup shape: {type, name, reason, path?}. Map to TodoItem.
    let kind = fu
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("question")
        .to_string();
    let name = fu
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let reason = fu
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(TodoItem {
        name,
        kind,
        status: TodoStatus::Pending,
        reason,
        depends_on: Vec::new(),
        coverage: String::new(),
        id: String::new(),
    })
}

fn build_instructions(has_lenses: bool) -> String {
    let mut s = String::from(
        "Update the todo list. Return JSON only:\n\
         {\"todo\": [{\"id\":\"ID\",\"type\":\"T\",\"name\":\"N\",\"reason\":\"R\",\
         \"status\":\"pending|done\",\"coverage\":\"C\",\"depends_on\":[\"ID1\",\"ID2\"]}]}\n\n",
    );
    s.push_str(
        "REPRIORITIZE — every call, not just when new items arrive:\n\
         - Sort all pending items so the one MOST LIKELY to surface a \
         bug OR most advance the investigation sits first. Subsequent \
         positions descend in expected payoff.\n\
         - 'Payoff' means: likelihood of finding an exploitable bug, \
         resolving an open question from a prior analysis, or unblocking \
         many downstream items (a shared dependency).\n",
    );
    if has_lenses {
        s.push_str(
            "- A 'lenses' array is provided — these are the \
             session-wide analytic frames every task's slow agent \
             applies in parallel. Rank each pending item by its \
             payoff across ALL lenses combined, not against just one. \
             Items that feed multiple lenses outrank items that feed \
             only one.\n",
        );
    }
    s.push_str(
        "- Put this reordering in effect by emitting the pending items \
         in the order you want them executed. The scheduler processes \
         them top-down.\n\
         - Tied payoffs: prefer items with fewer dependencies and those \
         that cite files/symbols still cold (not already in any done \
         item's 'coverage').\n\n",
    );
    s.push_str(
        "DEDUP ALGORITHM — run this for EVERY item in new_followups:\n\
         1. From the followup's name+reason, list the target files, \
         symbols, line ranges, and section refs it would cover.\n\
         2. For each done item in current_todo, read its 'coverage' \
         field AND its name+reason. If the followup's targets are a \
         subset of, or heavily overlap (>=50%), what a done item \
         already covered, DROP the followup — do not emit it in the \
         output todo. Do not be clever about 'different angle' — if the \
         files and symbols match, it is a duplicate.\n\
         3. For each pending item in current_todo, apply the same \
         check. If the new followup overlaps, DROP it.\n\
         4. The 'analysis_citations' list tells you exactly which \
         file:line pairs the most recent analysis touched; use it to \
         decide which done-item coverage to update.\n\
         5. Only followups that introduce genuinely new files, \
         symbols, or analysis angles survive.\n\
         Emit the dropped followup ids/names nowhere — just omit them.\n\n",
    );
    s.push_str(
        "COVERAGE FIELD — required on every done item you emit:\n\
         - 1-2 sentences naming the concrete files, symbols, and \
         line ranges the analysis examined for that item, plus the \
         bottom-line finding.\n\
         - Example: 'Covered drivers/net/netkit.c:80-115 \
         (netkit_run, netkit_xmit, scrub path). Finding: scrub is \
         no-op when endpoints share netns (CVE-2020-8558 class).'\n\
         - If a done item already has a non-empty coverage field, \
         keep it verbatim unless the new analysis meaningfully extends \
         what it covered — in which case append one sentence.\n\
         - Do NOT leave coverage empty on done items. Future dedup \
         calls depend on it.\n\n",
    );
    s.push_str(
        "OTHER RULES:\n\
         - Each item gets a short unique id (use the name, shortened)\n\
         - KEEP all done items in the list — they prevent re-adding \
         equivalent work\n\
         - Mark items as done if the analysis addressed them\n\
         - Keep pending items that are still relevant\n\
         - Remove ONLY pending items that are no longer relevant\n\
         - Max 20 pending items (done items don't count toward the limit)\n\
         - PARALLELISM: most items can run in parallel. Only add \
         depends_on when an item truly requires another's results first.",
    );
    s
}

/// Extract the `todo` array from the agent's response text. Tries
/// strict JSON first, then brace-matching.
pub fn parse_todo_response(text: &str) -> Option<Vec<TodoItem>> {
    if let Ok(r) = serde_json::from_str::<TodoUpdateResponse>(text) {
        if let Some(items) = todo_list_from_value(r.todo) {
            return Some(items);
        }
    }
    // Try to find an embedded `{"todo": [...]}` object via brace-match.
    let bytes = text.as_bytes();
    let mut start: Option<usize> = None;
    let mut depth: i32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start.take() {
                        if let Ok(r) = serde_json::from_str::<TodoUpdateResponse>(&text[s..=i]) {
                            if let Some(items) = todo_list_from_value(r.todo) {
                                return Some(items);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn todo_list_from_value(v: Value) -> Option<Vec<TodoItem>> {
    let Value::Array(items) = v else {
        return None;
    };
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        match serde_json::from_value::<TodoItem>(it) {
            Ok(item) => out.push(item),
            Err(e) => {
                tracing::debug!(target: "kres_agents", "skipping malformed todo entry: {e}");
            }
        }
    }
    Some(out)
}

fn extract_text(resp: &kres_llm::request::MessagesResponse) -> String {
    let mut out = String::new();
    for block in &resp.content {
        if let kres_llm::request::ContentBlock::Text { text } = block {
            out.push_str(text);
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect()
}

#[derive(Debug, Serialize)]
#[allow(dead_code)]
struct _KeepsDepsOn {
    x: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_citations_finds_c_h_rs() {
        let text = "See mm/slab.c:123 and include/foo.h:45-60 for the trap. Also src/a.rs:9.";
        let c = extract_citations(text);
        assert!(c.contains(&"mm/slab.c:123".to_string()));
        assert!(c.contains(&"include/foo.h:45-60".to_string()));
        assert!(c.contains(&"src/a.rs:9".to_string()));
    }

    #[test]
    fn extract_citations_caps_at_200() {
        let mut text = String::new();
        for i in 0..300 {
            text.push_str(&format!("path{i}/a.c:{i} "));
        }
        let c = extract_citations(&text);
        assert!(c.len() <= 200);
    }

    #[test]
    fn dedup_tokens_catches_paths_and_idents() {
        let toks = dedup_tokens("Check drivers/net/foo.c and scrub_something helper");
        assert!(toks.contains("drivers/net/foo.c"));
        assert!(toks.contains("scrub_something"));
        // Common stopword ruled out.
        assert!(!toks.contains("there"));
    }

    #[test]
    fn dedup_tokens_skips_short_idents_and_stops() {
        let toks = dedup_tokens("the and for abc");
        // None of these are length >= 5 and non-stop.
        assert!(toks.is_empty());
    }

    #[test]
    fn parse_todo_response_plain_json() {
        let text = r#"{"todo": [{"name": "x", "type": "investigate", "status": "pending"}]}"#;
        let got = parse_todo_response(text).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "x");
    }

    #[test]
    fn parse_todo_response_embedded_object() {
        let text =
            r#"Here you go: {"todo": [{"name": "y", "type": "read", "status": "done"}]} bye."#;
        let got = parse_todo_response(text).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].status, TodoStatus::Done);
    }

    #[test]
    fn parse_todo_response_bad_json_returns_none() {
        assert!(parse_todo_response("not a json object").is_none());
    }

    #[test]
    fn assign_ids_populates_unique_ids() {
        let mut items = vec![
            TodoItem::new("investigate slab", "investigate"),
            TodoItem::new("investigate slab", "investigate"),
            TodoItem::new("read a.c", "read"),
        ];
        assign_ids(&mut items);
        assert!(!items[0].id.is_empty());
        assert_ne!(items[0].id, items[1].id);
        assert!(!items[2].id.is_empty());
    }

    #[test]
    fn fallback_dedup_preserves_existing() {
        let existing = vec![TodoItem {
            name: "scrub drivers/net/netkit.c".into(),
            kind: "investigate".into(),
            status: TodoStatus::Pending,
            reason: String::new(),
            depends_on: Vec::new(),
            coverage: String::new(),
            id: String::new(),
        }];
        let new_fu = vec![json!({
            "type": "investigate",
            "name": "check drivers/net/netkit.c scrubbing",
            "reason": "possible bug in netkit scrub"
        })];
        let merged = fallback_dedup(&existing, &new_fu);
        // Overlapping tokens (drivers/net/netkit.c) → dropped.
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn fallback_dedup_keeps_distinct() {
        let existing = vec![TodoItem::new("one", "investigate")];
        let new_fu = vec![json!({
            "type": "investigate",
            "name": "completely unrelated subsystem query",
            "reason": "reason"
        })];
        let merged = fallback_dedup(&existing, &new_fu);
        assert_eq!(merged.len(), 2);
    }
}
