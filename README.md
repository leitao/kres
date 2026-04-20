# kres

Kernel code RESearch agent — an LLM-driven multi-agent REPL for
reviewing, auditing, and finding bugs in C source trees (the kernel
is the primary target).

## Quick start

1. Build:
   ```
   cargo build --release
   ```

2. Populate `~/.kres/` from this repo's shipped configs by running
   `setup.sh`:
   ```
   ./setup.sh --fast-key $FAST_API_KEY --slow-key $SLOW_API_KEY
   ```
   Each `--fast-key` / `--slow-key` argument accepts either a literal
   API key string or a path to an existing key file (contents trimmed
   and used verbatim). Running `setup.sh --help` lists the full set
   of options.

   The script copies `configs/*.json`, `configs/prompts/`, and
   `skills/` into `~/.kres/`, substitutes `@FAST_KEY@` / `@SLOW_KEY@`
   placeholders in the installed agent configs with the keys you
   passed, and installs `mcp.json` only when `semcode-mcp` is found
   on your `PATH` (or you pass `--semcode PATH`). It also installs
   the kernel skill if it can find a review-prompts tree — pass
   `--review-prompts /path/to/review-prompts` if you want that on
   from the start.

   Model selection lives in `~/.kres/settings.json`, one key per agent
   role (`fast`, `slow`, `main`, `todo`). `setup.sh` writes that file
   from its own flags:
     - `--slow MODEL` sets the slow-agent model (default
       `claude-opus-4-7`).
     - `--model MODEL` sets the fast / main / todo model (default
       `claude-sonnet-4-6`).
   The shipped agent configs do not hardcode a model; `settings.json`
   is the single source of truth. An operator who adds `"model": …`
   back to a specific agent config will override `settings.json` for
   just that agent (see the precedence note below).

   Running `--slow` and `--model` against the same model id is fine
   and often what you want if you only have one model's credentials.
   The difference between "fast" and "slow" work is driven by the
   per-agent system prompts shipped under `configs/prompts/` and the
   amount of context each agent receives, not by the model choice —
   so pointing both at the same id still produces the full
   fast/main/slow pipeline, each agent thinking as hard or as lightly
   as its prompt asks. Using two different models is an optimisation
   for cost or latency, not a correctness requirement.

   `--overwrite` is required to replace any file that already exists
   under `~/.kres/`; without it `setup.sh` is idempotent and reports
   each skipped file.

3. Run a review from a kernel tree:
   ```
   cd linux
   kres --results review --prompt 'review: fs/btrfs/ctree.c' --turns 2
   ```

The `--prompt 'review: fs/btrfs/ctree.c'` form is a two-part prompt:
the token `review` names a template at
`~/.kres/prompts/review-template.md`, and the rest of the string is
the specific target. kres splices the target onto the front of the
template to produce a full prompt covering object lifetime, memory
safety, bounds checks, races, and general bugs in the named code.
Drop a new `<word>-template.md` in `~/.kres/prompts/` to add your
own prompt templates.

`--results review` tells kres where to keep the run's artifacts:
`findings.json` (plus `findings-N.json` history snapshots), the
running narrative `report.md`, and the rendered `bug-report.txt`
when `/summary` fires. Without `--results`, kres picks
`~/.kres/sessions/<timestamp>/` automatically.

## `--turns`: stopping the run

By default kres keeps the REPL open indefinitely — you drive work
interactively via slash commands. `--turns N` gives you a
time-boxed non-interactive run:

- `--turns 0` — the default. No auto-stop; the REPL runs until you
  `/quit` or `ctrl-c` out.
- `--turns 1` — stop after the first completed task. Useful for a
  single focused question where you just want the answer.
- `--turns N` — stop after N completed tasks. A "completed task" is
  a unit that ran all the way through fast → main (data gather) →
  slow and produced findings. Most meaningful reviews want N ≥ 2 so
  the todo list (populated after task 1) actually gets drained.

When kres hits the cap it

1. cancels any in-flight work,
2. runs `/summary` automatically, producing `bug-report.txt` in the
   results directory (or the current working directory when no
   `--results` was given), and
3. exits.

Remaining pending or blocked todo items are moved to the "deferred"
list; `/followup` shows them if you re-enter the REPL later.

## Flow of work between the agents

A task goes through three agents, all configured from
`~/.kres/`:

- **fast** (`fast-code-agent.json`): scopes the task, figures out
  what source kres needs to look at, and emits a structured brief.
  When it's ready it returns a list of "followups" — concrete fetch
  requests (grep, file read, semcode symbol/callchain, git log)
  that the main agent should run.

- **main** (`main-agent.json`): the data fetcher. It takes the
  fast agent's followups and dispatches them to local tools and to
  any MCP servers configured in `mcp.json` (semcode in particular).
  The output is funnelled back into the fast agent for another
  round. This fast↔main loop runs until the fast agent says
  `ready_for_slow`, or the `--gather-turns` cap is reached.

- **slow** (`slow-code-agent-<tag>.json`, default `sonnet`): the
  deep analyser. It receives the gathered symbols and file sections,
  the cumulative findings from earlier tasks, and the task brief,
  then produces a new analysis and any new findings. Slow-agent
  output is cheap prose plus structured findings records.

- **todo** (`todo-agent.json`): after the slow agent returns, the
  todo agent dedups its followup suggestions against the existing
  pending/done todo list and emits an updated list. This is what
  drives larger reviews — see below.

- **merger**: a non-agent fast-client call that merges the new
  task's findings into the cumulative findings list. Duplicates get
  folded; old findings that a new one supersedes get marked
  `invalidated`.

All inference happens over the Anthropic streaming API. Every
round-trip is logged to `.kres/logs/<session-uuid>/` so you can
inspect what each agent saw and replied.

## Building up a larger review

A single `--prompt 'review: fs/btrfs/ctree.c'` call seeds exactly
one task. That task's slow-agent response usually contains followup
suggestions like "investigate memory lifetime of the path argument"
or "check callers of btrfs_search_slot". The todo agent turns those
into todo items.

From there:

- **`/next`** runs the first pending todo item as its own task.
- **`/continue`** dispatches every pending todo item.
- **auto-continue**: when there are pending todos and no active
  tasks, kres launches `/continue` automatically after 5 seconds of
  idle. You can override the idle by typing anything, including
  `/stop`.

Each task feeds back into the same pipeline: fast → main → slow →
merger, plus the todo agent deduping any new followups against the
existing list. The goal agent (a special mode of the main-agent
model) periodically checks whether the original prompt has been
satisfied; if yes, work stops even if followups remain.

A full review of a substantial source file usually takes between 5
and 50 task runs, depending on how branchy the code is and how
aggressive the slow agent is about producing followup questions.
`--turns` caps that; `/quit` lets you bail out early and resume
later.

## Summary output

After each task, kres appends the slow agent's narrative to
`<results>/report.md` and rewrites `<results>/findings.json` with
the cumulative merged list (the prior turn's canonical file is
copied to `findings-N.json` first, so you have the history).

At the end of a run you get a plain-text bug report via `/summary`
(or automatically on `--turns` exit, or separately with
`kres --summary --results <dir>`). That run:

- Picks up `<results>/prompt.md` (saved on the first submit so
  subsequent `/summary` or `--summary` invocations know the original
  question), `<results>/report.md`, and `<results>/findings.json`.
- Uses the fast agent with the `bug-summary.md` prompt template
  (installed under `~/.kres/prompts/`) as a dedicated system prompt.
- Orders the resulting sections by `bug-severity` — `high` →
  `medium` → `low` → `latent` → `unknown` — with one section per
  bug, each led by `Subject:`, `bug-severity:`, and `bug-impact:`
  lines.
- Writes the result to `<results>/bug-report.txt` (or
  `bug-report.txt` in the current working directory if you did not
  pass `--results`).

You can point `--template PATH` at a custom file to override the
shipped summariser prompt without rebuilding.

## Config directory: `~/.kres/`

`kres repl` resolves every optional config path in this order:

1. explicit CLI flag (e.g. `--fast-agent /path/to/fast.json`)
2. same filename under `~/.kres/`

Default filenames looked up in `~/.kres/`:

| Flag              | Default under `~/.kres/`         |
|-------------------|----------------------------------|
| `--fast-agent`    | `fast-code-agent.json`           |
| `--slow` tag      | `slow-code-agent-<tag>.json`     |
| `--main-agent`    | `main-agent.json`                |
| `--todo-agent`    | `todo-agent.json`                |
| `--mcp-config`    | `mcp.json`                       |
| `--skills`        | `skills/`                        |
| `--findings`      | `findings.json`                  |

A missing file in `~/.kres/` is not an error — the "not configured"
branch fires as if the flag were absent.

The `history` file is always written to `~/.kres/history` regardless
of other flags; it holds readline line-edit history.

`~/.kres/settings.json` carries per-user default model ids per agent
role. `setup.sh --slow MODEL` / `--model MODEL` populate the slow
slot and the fast / main / todo slots respectively; default values
are `claude-opus-4-7` (slow) and `claude-sonnet-4-6` (the rest).

Model-id precedence at runtime (see
`kres-repl/src/settings.rs::pick_model`):
  1. The agent config's explicit `"model"` field when present.
  2. The matching `settings.models.<role>` string in
     `~/.kres/settings.json`.
  3. `Model::sonnet_4_6()` — the built-in fallback when both of the
     above are absent.

The shipped agent configs no longer set `"model"`, so in a fresh
install step 2 drives the actual choice. Reintroducing a `"model"`
line in one of the agent configs still takes effect and overrides
settings.json for that agent only.

## Workspace layout

```
kres/
├── Cargo.toml                     Rust workspace manifest
├── kres/                           binary crate (`kres` command)
├── kres-core/                      Task, TaskManager, shutdown, findings
├── kres-llm/                       Anthropic streaming client + rate limiter
├── kres-mcp/                       stdio JSON-RPC client for MCP servers
├── kres-agents/                    fast / slow / main / todo / consolidator / merger
├── kres-repl/                      readline UI, commands, signal handling
├── configs/                        per-agent JSON configs (shipped defaults)
│   ├── fast-code-agent.json
│   ├── slow-code-agent-opus.json
│   ├── slow-code-agent-sonnet.json
│   ├── main-agent.json
│   ├── todo-agent.json
│   ├── settings.json
│   ├── mcp.json
│   └── prompts/                   system prompts + review templates
├── skills/                         domain-knowledge markdown fed to agents
│   └── kernel.md
├── docs/                           JSON-schema docs for agent wire formats
│   ├── findings-json-format.md
│   ├── prompt-json-format.md
│   └── response-json-format.md
├── CLAUDE.md                       project instructions for Claude Code
├── setup.sh                        bootstrap ~/.kres/ from configs/
├── .githooks/pre-commit            runs cargo fmt + clippy on every commit
└── README.md
```

Build: `cargo build --release`
Test: `cargo test --workspace`
Lint: `cargo clippy --workspace --all-targets -- -D warnings`
Format check: `cargo fmt --all --check`

## Pre-commit hook

`.githooks/pre-commit` runs `cargo fmt --check` + `cargo clippy -D
warnings` on every commit. Enable it per-clone with:

```
git config core.hooksPath .githooks
```

## Supported CLI

```
kres test <key_file> [--prompt ...] [--model ...]
kres turn <key_file> -o <output.md> [-i <input.json>] [other flags]
kres [--fast-agent ...] [--slow TAG | --slow-agent ...] [--main-agent ...]
     [--todo-agent ...] [--mcp-config ...] [--skills DIR]
     [--results DIR] [--findings PATH] [--report PATH] [--todo PATH]
     [--prompt PROMPT] [--template PATH] [--turns N]
     [--gather-turns N] [--stop-grace-ms MS] [--stdio]
     [--summary]
```

Interactive REPL commands: `/help`, `/tasks`, `/findings`, `/stop`,
`/clear`, `/cost`, `/todo`, `/summary [FILE]`, `/report <path>`,
`/load <path>`, `/edit`, `/reply <text>`, `/next`, `/continue`,
`/quit`.
