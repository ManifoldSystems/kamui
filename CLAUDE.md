# Kamui Development Guide

This file is the engineering handoff and working agreement for AI agents contributing to Kamui.
Read `README.md` for user-facing documentation and `ROADMAP.md` for prioritized product phases.

## Product Direction

Kamui is a provider-agnostic LLM CLI written in Rust. It is evolving from interactive chat into a
repository-aware coding agent in the direction of Codex and Claude Code.

The near-term goal is not to build every possible AI feature. Prioritize a reliable coding workflow:

1. Safe read-only repository context.
2. A provider-agnostic tool-call protocol.
3. Permissioned file editing and command execution.
4. Efficient context management and additional providers.

Prefer small, complete capabilities over broad but incomplete systems. Challenge roadmap items whose
effort or operational risk is disproportionate to their immediate value.

## Current Product Behavior

- Every normal launch starts a new chat. Resume must be explicit with `/resume <id>` or
  `kamui -r <id>`.
- `kamui -p <prompt> [--auto-approve]` runs one prompt non-interactively through the same agent
  loop as interactive chat (including tools) and exits. The exchange is persisted as a resumable
  session exactly like an interactive turn, including a generated title. There is no REPL loop, no
  stdin reader, and no spinner. Tool calls that require confirmation are denied by default (the
  model is told the refusal is due to non-interactive mode); `--auto-approve` runs them without
  prompting instead.
- Sessions are created lazily after the first successful streamed response. Empty chats are not
  persisted or listed.
- Completed user/assistant exchanges are stored in SQLite. Partial responses from interrupted or
  failed streams are not added to history.
- The first completed exchange receives an AI-generated title. Title-generation usage is recorded
  with kind `title`, while the request count shown to users counts only primary chat requests.
- Each chat usage row records the model that produced it (`user_version = 5`). `/stats` shows the
  session totals and, when a session used more than one model, a per-model token breakdown.
- Streaming deltas are printed immediately. Usage and finish reason are shown after completion. On
  a real interactive TTY, a braille spinner runs until the first token and tool outcomes report
  status, duration, and output size. Pipes and `-p` remain plain and free of cursor-control output.
  The fullscreen Ratatui transcript (opencode-style: thick-border message rails, sidebar rail,
  editor box, token badge) is driven by `InputHub` — one persistent keyboard thread that owns all
  input. The editor stays live while the agent runs (typed lines queue and run after the turn),
  Esc/Ctrl+C raise interrupts raced at every await point, approval/ask_user answers flow through
  requester channels, and Ctrl+K/Ctrl+S/? plus bare /model, /sessions and /help open modal
  dialogs that submit real slash commands. The braille spinner animates in the editor row via a
  background ticker redrawing through the shared terminal mutex; Ctrl+C at an idle prompt needs a
  double press within three seconds. ratatui 0.30 strips embedded newlines from Span content at
  construction, so multi-line notices are split into separate Lines before building Text. Skill
  loading is lenient like opencode: any visible folder may host a skill, missing frontmatter
  fields fall back to folder name / first body line, hidden dot-folders are skipped silently.
- `Ctrl+C` while a turn is in flight (waiting for the model, streaming, at an approval prompt, or
  running a command) interrupts that turn and returns to the prompt; the partial turn is discarded
  and not saved, and an interrupted command is killed via `kill_on_drop`. `Ctrl+C` at the idle
  prompt shuts down gracefully. Windows stdin uses a reader thread and Tokio channel so the async
  runtime does not block on terminal input.
- Supported chat commands are `/help`, `/new`, `/sessions`, `/resume <id>`, `/model [name]`,
  `/rename <id> <title>`, `/search <text>`, `/compact`, `/undo`, `/jobs`, `/index`, `/commands`,
  `/delete <id>`, `/stats`, `/usage`, `/status`, `/memory`, `/forget <text>` (or `/forget all`),
  and `/exit`. Plain `exit` also quits.
- Users define their own slash commands as markdown files (`src/commands.rs`): global ones in
  `<config dir>/kamui/commands/*.md`, project ones in `<project>/.kamui/commands/*.md`, with a
  project command shadowing a global one of the same name — the same precedence `kamui.toml`
  already uses. Invoking `/<name>` substitutes the file's body as that turn's prompt and appends
  any trailing text, so `/review @src/a.rs` still goes through normal `@file` expansion. This is
  text substitution only: it grants no capability and bypasses no approval, making its risk
  identical to the `KAMUI.md` instructions that already ship. Optional frontmatter supplies a
  `description` for `/commands`; unknown keys are ignored so a file written for another tool works
  unchanged. Files named after a built-in command are skipped (`commands::RESERVED`, kept in sync
  with `chat::print_help`). Works in `-p` as well as interactive chat. There is deliberately no
  chaining between commands — each is invoked independently.
- Streamed responses are rendered with a small markdown-to-ANSI subset (`src/markdown.rs`):
  headings, `**bold**`, `` `code` ``, fenced blocks, list markers, blockquotes. Rendering is
  line-buffered — a line is styled only once its newline arrives — so streaming still appears live
  while each line can be styled as a whole; `chat.rs` flushes the buffer on stream end *and* on
  interrupt so a partial line is never swallowed. Raw markdown (not the styled text) is what gets
  persisted and re-sent to the model. `_italic_` and single-`*` italics are deliberately
  unsupported: they would corrupt `snake_case` identifiers and globs like `src/*.rs`. Styling is
  disabled when stdout is not a terminal, or when `NO_COLOR` is set.
- `/stats` reports the active session; `/usage` aggregates every session by day and by month
  (`storage::usage_by_day`/`usage_by_month`/`usage_total`). Both follow the same convention: the
  request count covers only `kind = 'chat'` rows while token sums include every kind, so title
  generation is counted in spend but not in request totals.
- Cost reporting is opt-in and needs no migration: `usage_records` already carries the tokens and
  the `model`, so only prices are missing, and only the user can supply them (`[pricing]` in
  `kamui.toml`, `src/pricing.rs`). With no prices configured both reports are unchanged — no cost
  column, no zeroes, and no per-model query is even run. With prices configured `/stats` gains a
  `Cost:` line (over every usage kind, title generation included) and a cost cell per model row,
  and `/usage` gains a cost column per period. A model with usage but no configured price is never
  reported as free: its cell reads `unpriced`, and a total that mixes priced and unpriced usage
  carries a trailing `+` plus an explanatory note.
- Long sessions are compacted automatically: when the un-summarized recent history exceeds a byte
  threshold (about half the profile's `context_window`, or a default), older messages are folded
  into a rolling summary and the request sends the summary plus recent messages. `/compact` forces
  it. Full history stays in storage; the summary and summarized boundary are persisted per session
  and restored on resume, so a restart does not generate a different cache epoch unnecessarily.
- `/model` lists the configured provider profiles and marks the active one; `/model <name>` switches
  the active provider and model, rebuilding the provider and persisting the choice in the SQLite
  `settings` table so it survives restarts. The banner shows the active model and profile. In
  fullscreen TUI mode bare `/model` opens the picker dialog, whose "+ Add provider / model" entry
  reuses onboarding (base URL + API key -> live list_models -> pick) to append a new profile to
  the global kamui.toml via `config::append_profile` and switch immediately.
- After each streamed response the usage line reports time-to-first-token and total response time.
  These latency figures are displayed only, not persisted.
- Chat requests offer the model read-only `read_file`, `list_directory`, `grep`, `glob`, and
  `update_plan` tools, `command_status`/`stop_command` for background jobs, plus a
  confirmation-gated `run_command` tool. When the model calls one, Kamui runs a bounded streaming
  agent loop: it executes the tool, prints a one-line trace (or, for `update_plan`, a rendered
  `[ ]`/`[~]`/`[x]` checklist), feeds the result back, and continues until the model returns a
  plain answer. The whole turn is persisted, including the tool requests and results, so resumed
  sessions replay them.
- `run_command` never runs unattended by default. Kamui prints the requested command and prompts
  `[y/N/a]`; `y`/`yes` approves once, declining feeds a refusal back to the model. Three ways to
  skip the prompt: `a`/`always` — ported from the sibling Kumo project's "Always allow" button,
  adapted from Telegram inline buttons to a plain third answer — both approves that call and grants
  the *tool* (not that specific command) a standing pass in `start_chat`'s `always_allowed:
  HashSet<String>` for the rest of the active session, cleared on `/new` or deleting the active
  session (`handle_command` takes `&mut HashSet<String>` for this); a global-only `[permissions]
  allow_commands = [...]` exact-match allowlist (`Tool::requires_confirmation_for`, checked instead
  of the no-arg `requires_confirmation` at dispatch time), configured ahead of time rather than
  granted from the prompt; or launching with `--auto-approve` (accepted by the bare `kamui` command
  too, not just `-p`), which prints a startup banner and skips every confirmation prompt —
  including `[permissions]`-unlisted commands and `patch_file` — for the whole session. Commands run
  in the project directory with stdin disabled, a configurable kill timeout
  (`[commands].timeout_secs`, default 30 seconds), and a 16 KiB output cap, and the result includes
  the exit code with captured stdout and stderr.
- `run_command(background: true)` starts a job without waiting for it: a `tokio::task`
  (`run_background_job`) owns the child process, drains its stdout/stderr independently so a killed
  or timed-out job still reports whatever it produced, and races `child.wait()` against a
  `[commands].background_max_secs` safety timer (default 30 minutes — a backstop against a runaway
  process, not a limit on legitimate long-running commands) and a `tokio::sync::watch` kill signal.
  `command_status` (omit `job_id` to list every job) and `stop_command` read/signal the shared
  in-memory `JobRegistry`; `/jobs` reads it directly without going through the model. Jobs do not
  survive a restart and are killed on shutdown (`tools::kill_all_jobs`, called from `chat::shutdown`
  and the end of `run_once`) so nothing outlives the process.
- When the external `rtk` binary is available (detected once per process), simple approved commands
  are prefixed with `rtk` to compress their output. Commands with shell operators, commands already
  prefixed with `rtk`, and all commands on systems without RTK run directly. So does anything whose
  first word the shell would not resolve to a program — a `cmd`/`sh` builtin (`tools::CMD_BUILTINS`
  and `tools::SH_BUILTINS`, kept per-shell because `echo` is a `cmd` builtin but a real `/bin/echo`
  on Unix) or a `VAR=value` assignment prefix — since `rtk` can only prefix a program and would
  otherwise turn a working `cd ..` into `Binary 'cd' not found on PATH`. The first result line
  records the exact command line executed.
- MCP servers declared as `[mcp.<name>]` in the global config are launched at startup over stdio;
  each advertised tool joins the registry as `<server>__<tool>` and requires approval per call unless
  the server sets `trusted = true`. A server that fails to start is reported and skipped. Project
  configs may not declare servers, since launching one is arbitrary code execution. Only stdio
  transport and the tools capability are supported.
- `patch_file` edits one file per call by exact-match replacement and shows a +/- preview before
  the same `y`/`yes` approval. `old_text` must match exactly once or the patch is rejected with
  recovery guidance; empty `old_text` creates a new file that must not exist. Matching is
  line-ending-agnostic (CRLF files are compared in LF space and rewritten with their original
  endings), so LF `old_text` still matches a CRLF file. Writes are atomic (temp file plus rename)
  and paths pass the same containment checks as reads.
- Approval stays per-file, but the chat loop snapshots each `patch_file` target's pre-turn content
  the first time it is touched in a turn (`chat::snapshot_patch_target`). If the turn is cancelled
  with `Ctrl+C` before it finishes, every file it already changed is reverted automatically
  (`chat::revert_on_cancel`/`revert_snapshot`) so a multi-file edit can never be left half-applied
  with no trace in session history. `/undo` performs the same revert for the most recently
  *completed* turn — one level, in-memory only (not persisted to SQLite), cleared after use or when
  a new turn starts.
- Session IDs may be resolved from an unambiguous prefix. The UI normally displays the first eight
  characters.
- Resume displays the six most recent messages and reports how many earlier messages were omitted.
- Context percentage is displayed only when `KAMUI_CONTEXT_WINDOW` is configured.
- `ask_user` lets the model pause a turn to ask a clarifying question (with up to 4 optional
  numbered choices) instead of guessing. In interactive chat it reads one line from the same
  stdin channel used for command input and approval answers; a numbered reply resolves to that
  option's text, anything else is returned as typed. In `-p` non-interactive mode there is no user
  to ask, so it is declined with a message telling the model to proceed on its own judgment. It is
  intercepted by the chat loop before `ToolRegistry::dispatch`, since it needs the interactive
  stdin channel that `Tool::run` has no way to receive.
- `spawn_agent` delegates a self-contained task to an isolated sub-agent (`chat::run_spawned_agent`)
  and returns only its final text, so the sub-agent's own tool trace never enters the parent
  conversation. Intercepted the same way `ask_user` is, since it needs the active `Provider`/model
  and `ProjectContext` directly. The sub-agent gets a fresh system prompt, no shared history with
  the parent, and runs against `ToolRegistry::read_only` (`read_file`/`list_directory`/`grep`/
  `glob` only — no `run_command`, `patch_file`, memory tools, or recursive `spawn_agent`), so none
  of its tool calls ever need confirmation and it cannot mutate anything. Independent calls emitted
  in one response run concurrently through `chat::dispatch_spawn_agents` in batches capped at four;
  outputs return in original tool-call order. Each invocation derives a sibling sticky ID from its
  tool-call ID and reuses it for every round, isolating its prompt cache from the parent and other
  sub-agents. The parent waits for the batch, and no parallel approval flow is needed because every
  sub-agent tool is read-only.
- `/index` rebuilds the semantic-search index (`chat::run_index`): walks the project the same
  `.gitignore`-aware way `grep`/`glob` do, splits each file into roughly 50-line chunks
  (`tools::chunk_text`, preferring blank lines and common declaration starts near the target),
  skips any file whose content hash (`chat::content_hash`, a non-cryptographic
  change-detection hash) matches what was stored last run, embeds the rest via the active profile's
  `Provider::embed`, and removes chunks for files no longer present. Requires
  `Profile::embedding_model` to be set; otherwise it fails with a clear message and `search_code` is
  never offered to the model. `search_code` (also intercepted like `ask_user`, since it needs
  `Provider`/`Database`) embeds the query, probes SQLite FTS5 and nearby LSH embedding buckets, then
  exact-cosine reranks the bounded candidate union. Projects up to 2,000 chunks use exhaustive
  ranking for maximum recall. The index is scoped per project by
  `ProjectContext::key` (the canonical root path), so several projects share the one global
  database without colliding.
- Files already in the index are kept current automatically: after a turn is persisted,
  `chat::refresh_index_for_paths` re-embeds every `patch_file` target that the index already knows
  about (`chat::index_file` is the shared write path, used by `/index` too). A path the turn
  *created* is deliberately not added — stale text in an indexed file makes `search_code` quote
  code that no longer exists, while an unindexed file is merely absent from results, so only the
  correctness problem justifies unasked spend. A target that has since become unreadable has its
  chunks dropped rather than re-embedded. Interactive chat takes the path set from its revert
  snapshot; `-p` collects `tools::patch_target` results directly, since it has no snapshot. Both
  run only after `save_turn`, and a failure is reported without failing the turn.
- Building the index is never automatic. Cursor-style index-on-open is deliberately not adopted:
  Kamui is a start-and-exit CLI, not a long-lived editor, so there is no background window to hide
  the work in, and embedding spends the user's own API key — silent startup spend is the wrong
  default. Instead,
  `chat::index_staleness` reports drift: when the project has an existing index, the interactive
  banner prints how many files changed, appeared, or disappeared and suggests `/index`. It compares
  each file's mtime against `indexed_files.indexed_at` rather than re-reading and hashing content,
  so it costs one directory walk, no file reads, no network, and no spend. That makes it a hint (a
  checkout can bump an mtime without changing content), which is acceptable because `/index` still
  does the authoritative hash comparison before re-embedding. Nothing is printed when the index is
  fresh, when the project was never indexed, when no `embedding_model` is configured, or in `-p`
  mode (that output is script input); a failed check is swallowed rather than blocking startup.
- `remember`, `update_memory`, and `forget` manage a global memory table (`user_version = 6`),
  intercepted the same way `ask_user` is (they need `Database` directly). Unlike project
  instructions (`KAMUI.md`, a file the user edits) or session history (scoped to one conversation),
  a remembered fact is global and permanent: it is read fresh from storage on every turn (not
  frozen at startup, since Kamui is a single interactive process rather than a server shared
  across many chats) and is visible in every project Kamui is run in afterward. `update_memory` and
  `forget` resolve their target via an unambiguous case-insensitive substring match, failing with
  guidance on zero or multiple matches rather than guessing. Total stored memory is capped at 4
  KiB; `/memory` lists what is remembered and `/forget <text>` (or `/forget all`) removes it
  directly, without going through the model.
- `kamui doctor` checks configuration, chat and embedding endpoints, MCP server connections, project
  discovery/instructions, SQLite integrity/schema, and optional RTK availability one at a time. It
  exits non-zero if any required check fails, so it is usable as a pre-flight script.
- `kamui benchmark <suite.json> [--profile <name>] [--runs <n>]` runs repeatable prompt cases,
  validates optional case-insensitive expected substrings, reports latency/token totals, and exits
  non-zero when a case fails. For Orvix Coding profiles, runs are append-only turns under one stable
  session ID per case and include steady-state cache metrics with each case's first turn excluded.
- `kamui jobs` manages a SQLite-backed scheduled command queue (`src/jobs.rs`, schema v9). One-shot
  and interval jobs persist across restarts; a foreground worker atomically claims due work, stores
  capped output/exit status, coalesces missed intervals, and can run once under an OS scheduler.
- `kamui status` (`main::run_status`) prints a config/database summary with no network calls at
  all — profile, model, base URL, all configured profiles, `embedding_model`, MCP server names,
  the `[permissions]` allowlist, project and database paths, session count, memory count, and the
  active project's indexed-chunk count. Ported from the sibling Kumo project's `kumo status`
  (read-only summary, distinct from `doctor`'s active checks), adapted from Kumo's
  single-workspace/single-provider config to Kamui's per-project root and multi-profile config.

## Repository Context

The process working directory is the project root.

- Every request begins with an agentic system prompt (`src/prompt.rs`) that tells the model how to
  work as a terminal coding agent; its tool-usage guidance is included only when the active profile
  offers tools. At startup, Kamui loads `KAMUI.md` if present, otherwise `AGENTS.md`, and appends
  that content to the system prompt.
- `CLAUDE.md` is an agent development guide and is intentionally not loaded by Kamui at runtime.
- A prompt can attach UTF-8 text with a relative reference such as `@src/main.rs`; paths containing
  spaces use quoted references such as `@"docs/My Notes.md"` or `@'docs/My Notes.md'`.
- Referencing a directory (`@src`) attaches the text files inside it, walked with the `ignore` crate
  so `.gitignore`, global excludes, and hidden files are honoured (`require_git(false)`, so rules
  apply outside a repository too). Attachment stops at the shared context budget or 50 files;
  leftovers are reported in a trailing note rather than failing the prompt.
- Each file is limited to 1 MiB and all attached context is limited to 2 MiB per request.
- Absolute paths, directories, binary/non-UTF-8 files, and paths or symlinks outside the project root
  are rejected.
- Duplicate references are attached once.
- Expanded file contents are sent for that request only. The original user prompt, not expanded
  contents, is stored in session history.
- `@diff` attaches raw unstaged tracked changes using `git diff`.
- `@staged` attaches raw staged changes using `git diff --cached`.
- `@clipboard` attaches the system clipboard (via `arboard`): text when present, otherwise clipboard
  image data encoded as PNG, so a screenshot can be pasted without saving a file. It errors clearly
  when the clipboard is unavailable (headless) or holds neither. Reading the real clipboard is
  environment-dependent and has no unit test; the PNG encoding is tested directly.
- An `@` reference ending in `.png`, `.jpg`, `.jpeg`, `.gif`, or `.webp` is attached as an image
  (base64 `ImageAttachment` on the message), not inlined as text. Images pass the same containment
  checks, are capped at 5 MiB each, are sent only for that request, and require a vision model. The
  OpenAI adapter switches message content to a parts array only when images are present.
- Untracked files are not in `@diff`; users must attach them explicitly with `@path`.
- Raw diff is deliberate. Do not silently replace it with a condensed representation because code
  review may require details omitted by summarization.

## Architecture

Important modules:

- `src/main.rs`: CLI argument parsing, configuration loading, dependency construction, and startup.
- `src/config.rs`: `kamui.toml` discovery, global/project layering, named provider profiles, and the
  first-run scaffold. `global_config_dir` is the shared lookup for the OS config directory Kamui
  owns, reused by `commands` for the global command folder beside `kamui.toml`.
- `src/commands.rs`: user-defined slash commands loaded from markdown files, global and
  project-local, with built-in names reserved.
- `src/benchmark.rs`: repeatable JSON benchmark suites, expected-text validation, and aggregate
  latency/token reporting.
- `src/jobs.rs`: persistent one-shot/interval command queue CLI and foreground worker.
- `src/markdown.rs`: line-buffered markdown-to-ANSI rendering for streamed output, disabled when
  stdout is not a terminal.
- `src/terminal.rs`: shared terminal capability detection and tool lifecycle formatting; protects
  piped/non-interactive output from ANSI and spinner control sequences.
- `src/pricing.rs`: the optional `[pricing]` table, cost tallying, and the `unpriced`/`+` rendering
  rules that keep an unpriced model from reading as a free one.
- `src/prompt.rs`: the agentic system prompt, combined with project instructions per request.
- `src/compaction.rs`: rolling-summary context compaction (threshold, message selection, summary
  request); the chat loop drives it automatically and via `/compact`.
- `src/cache.rs`: the Coding cache contract - request assembly order (`turn_messages`), the volatile
  tail, `prefix_bytes`/`PrefixGuard` drift detection, and the per-session hit report.
- `src/mcp.rs`: MCP client over stdio via the `rmcp` SDK; wraps each server tool as a Kamui `Tool`.
- `src/chat.rs`: interactive loop, streaming display, session commands, title generation, the
  streaming tool agent loop, graceful shutdown, and `run_once` for non-interactive `-p` prompts.
- `src/context.rs`: project instruction discovery and safe `@file`, `@diff`, and `@staged`
  expansion, including the shared `read_project_file` path-safety helper.
- `src/tools.rs`: the async `Tool` trait, `ToolRegistry` dispatch, the read-only `read_file`,
  `list_directory`, `grep`, `glob`, `update_plan`, `command_status`, and `stop_command` tools, and
  the confirmation-gated `run_command` and `patch_file` tools. `run_command`'s background-job
  registry (`JobRegistry`, `JobEntry`, `run_background_job`) also lives here, as does
  `ToolRegistry::read_only` (the restricted registry `spawn_agent`'s sub-loop runs against) and
  `chunk_text`/`search_code_definition` (the boundary-aware chunker and conditionally-offered
  `search_code` definition that `chat::run_index`/`chat::dispatch_search_code` use).
- `src/provider/mod.rs`: provider-independent request, response, message, usage, and streaming types.
- `src/provider/openai.rs`: OpenAI-compatible Chat Completions HTTP and SSE implementation.
- `src/storage.rs`: SQLite schema/migrations, sessions, messages, usage, scheduled jobs, and the
  hybrid `/index` store (`code_chunks`, FTS5, LSH buckets, and `indexed_files`).
- `.github/workflows/release.yml`: tag-triggered multi-platform release builds.
- `install.ps1` and `install.sh`: release-binary installers with SHA-256 verification.

Keep the core provider-agnostic. Provider-specific payloads and parsing belong under the provider
implementation. Do not leak OpenAI response structures into chat, storage, context, or future tool
runtime APIs.

The current `Provider` trait supports non-streaming `chat` and streaming `chat_stream`, plus `embed`
(a batch text-to-vector call for `/index`/`search_code`, `src/provider/openai.rs` via the
OpenAI-compatible `/embeddings` endpoint). The non-streaming `chat` path is used for title
generation and is the intended path for tool-calling turns. `embed` is only ever called when the
active profile's `Profile::embedding_model` is set.

The provider-agnostic tool-call protocol is modeled in `provider/mod.rs` as `ToolDefinition`,
`ToolCall`, and tool-request/tool-result `Message` variants; `ChatRequest` carries `tools` and both
`ChatResponse` and `StreamEvent::Done` surface `tool_calls`. The OpenAI adapter maps these to and
from wire types entirely within `provider/openai.rs`, including index-keyed reassembly of tool calls
streamed across deltas, so the core no longer serializes its own types into an OpenAI-shaped payload.
Native Anthropic and Gemini adapters must reuse these same neutral types.

Tools live in `src/tools.rs`. `ToolRegistry` holds boxed async `Tool` implementations, exposes their
`ToolDefinition`s, and dispatches a `ToolCall` by name, returning any failure as an `Error: ...`
string so the model can recover rather than aborting the turn. Read-only `read_file`,
`list_directory`, `grep`, and `glob` reuse `context::resolve_within_root` for path safety and the
`ignore` crate's `.gitignore`-aware walking (also used by `@dir` expansion); `run_command` executes
shell commands with a timeout and output cap. Permission policy stays in Kamui, not the tools: a tool
advertises `requires_confirmation`, and the chat loop prompts the user before dispatching any such
call, feeding a refusal back to the model if declined. The chat loop runs a streaming agent loop
bounded by `MAX_TOOL_ROUNDS`: it streams a turn, and if the model requested tools it records the
request, runs each tool, appends the results, and requests again until a plain answer arrives.

Whole turns are persisted, including tool messages. A `user_version = 3` migration rebuilds the
`messages` table to allow the `'tool'` role and store `tool_calls` (JSON) and `tool_call_id`;
`save_turn` writes the prompt, tool requests, tool results, and final answer atomically, so resumed
sessions replay tool interactions. Recorded usage is still the final round's, not the whole turn's.
The terminal runner, mutation tools, per-turn usage accounting, and a durable audit trail remain.

## Coding Cache Contract

Orvix Coding Plan profiles (`send_session_id = true`, `completions_path = "/coding/completions"`)
pin a session to one upstream worker that caches the request prefix. A cached prefix only pays off
while the bytes *before* the newest turn are identical, so Kamui's request assembly is a contract,
not an implementation detail. It lives in `src/cache.rs`; the chat loop and `run_once` both build
their turn through `cache::turn_messages`, which is the single definition of the order.

Every request is assembled as:

```text
[system]  frozen head     base prompt + tool guidance + project instructions
[system]  frozen head     eager skill list (omitted when there are no skills)
[...]     history         the un-summarized conversation, unchanged
[system]  volatile tail   memory snapshot + running compaction summary
[user]    the new turn
```

- **The head is built once per session and cloned per turn** (`chat::build_head_messages`). Its
  inputs are fixed at startup (`KAMUI.md`, the profile's `tools` flag) or change only on an explicit
  user action - `/model`, `/skills toggle` - and each of those resets `head_messages`. One block per
  message rather than one concatenated string: the bytes on the wire are the same either way, but it
  keeps each block's boundary visible to a provider that caches at one.
- **`cache::PrefixGuard` records the head and tool bytes each turn** and prints a notice when they
  move, so a collapsed hit rate always has a stated cause. It never rewrites a request: a user who
  toggles a skill gets the skill.
- **Memory and the summary ride in the tail**, behind the history, not in the head. They are read
  fresh - a fact remembered this turn is visible on the next one, which is the shipped behaviour -
  and putting them in front would mean one `remember` call moved byte zero and re-read the whole
  conversation at full price. Behind the history, the divergence point is only ever the previous
  turn's tail. `memory_dirty` makes the tail re-read the database only after a memory tool ran,
  which saves a round trip per turn; it is an efficiency, not a cache protection, because the tail
  is free to change.
- **Tool definitions are computed once per session and reused** (`session_tools`), so the wire
  `tools` array cannot change shape mid-session. Pending Plan Mode no longer shrinks the roster:
  mutating calls are refused at execution time with an explanatory message (`is_mutating_held`)
  instead. `prefix_bytes` folds each definition's name, description, and parameter schema, in order,
  into the guarded bytes, so a future reordering cannot pass unnoticed.
- **`prompt_cache_key`** carries the session id (clamped to 64 chars) as a top-level field on
  OpenAI-compatible bodies, so a backend doing prefix caching routes the session consistently.
  `skip_serializing_if` omits it when there is no session, so a generic backend sees no wire change.
- **Compaction waits longer under the contract.** `compaction::threshold` compacts at ~50% of the
  context window normally but ~85% for a cache-pinned profile: dropping folded-away messages makes
  every later request a fresh prefix, so the token saving is charged back as a full re-read. It is
  still allowed - the context window is a hard limit and a cache miss beats a rejected request -
  and the notice says the prefix resets.
- **Observability, two halves.** Before the request, `PrefixGuard` catches drift by comparing bytes
  - deterministic, and it names the turn that broke. After the response, `chat::cache_miss_label`
  compares this round's cached tokens against the previous one (1024-token noise floor) and names
  the likely cause (`miss`, `miss (model switch)`, `miss (prefix rebuilt)`); it is silent on hits
  and first turns. The usage line shows whichever applies, falling back to `Cached: 0 (warm-up)` on
  a pinned profile so a zero is never mistaken for a provider that reports nothing. `/stats` then
  reports the session: `median X% over N turns | >=90%: A% | >=95%: B% | warm-up: C`. Turn one is
  excluded from the ratios - it cannot hit a cache that does not exist - while a later warm-up turn
  stays in the denominator, because a prefix that churned mid-session is exactly the failure worth
  seeing. `storage::cache_samples` feeds it from `kind = 'chat'` rows only.

Known gap closed (2026-09-04): title generation and compaction used to share the conversation's
sticky `session_id`, which could evict the warm prefix on the Coding Plan worker. They now send a
derived sibling id (`{session}:title` / `{session}:compact` via `cache::side_session_id`) so Orvix
still receives a required `session_id` while the conversation's Redis sticky key and
`prompt_cache_key` stay untouched. Symptom that should stop: turn two reporting `Cached: 0` right
after title generation while later turns are fine.

## Storage Decisions

- SQLite is compiled with the `bundled` feature so end users do not install SQLite separately.
- Use schema migrations through `PRAGMA user_version`; do not make destructive schema assumptions.
- The default database is in the operating system local application data directory under `kamui`.
- `KAMUI_DATA_DIR` overrides the data directory for servers and containers.
- Multi-device synchronization, if built, should exchange records through an API. Do not synchronize
  by copying a live SQLite database.
- Foreign keys and cascading session deletion must remain enabled.
- Save an exchange and its usage atomically.
- `code_chunks`/`indexed_files` are project-scoped inside the one global database: every row carries
  a `project` column holding `ProjectContext::key` (the canonical project root), because `path` is
  project-relative and would otherwise collide across projects. Every accessor takes the project key
  as its first argument; there is no unscoped variant, so a new call site cannot forget it. The
  `user_version = 8` migration recreates both tables (`indexed_files`' primary key becomes
  `(project, path)`, which SQLite cannot alter in place) and drops any pre-existing rows: they
  record no project and cannot be attributed to one. That is safe here specifically because the
  index is a regenerable cache, not user data — do not use it as precedent for dropping rows from
  `sessions`, `messages`, `usage_records`, or `memory`.
- `user_version = 9` adds `scheduled_jobs`; queue rows are durable user state and must never be
  dropped during migration. Atomic `UPDATE ... RETURNING` claims prevent duplicate execution by two
  workers. Running jobs left by a crashed worker become `interrupted`, never silently retried.
- `user_version = 10` adds `indexed_files.embedding_model`, `code_chunks.lsh_bucket`, and the FTS5
  mirror. `replace_file_index` swaps a fully prepared file index transactionally. Search uses full
  cosine scoring below 2,000 chunks and bounded FTS/LSH candidate scoring above it.

## Configuration

Provider and model settings come from `kamui.toml` files; `src/config.rs` owns loading. No
environment variables participate in provider or model configuration — `.env`/dotenvy support was
removed. Precedence is:

1. Project `kamui.toml` in the working directory (non-secret only).
2. Global `kamui.toml` in the OS configuration directory (may hold the API key).
3. Built-in defaults.

Two forms are accepted. The flat form sets top-level `model`/`context_window` and a `[provider]`
section. The multi-profile form defines named `[profiles.<name>]` entries (each with `model` and
optional `base_url`/`api_key`/`context_window`/`tools`) plus a `default_profile`; profiles win when
present, and `/model` switches between them at runtime with the active choice stored in the SQLite
`settings` table. A single implicit profile named `default` backs the flat form. A profile may set
`provider = "<name>"` to inherit `base_url`/`api_key`/`tools` from a shared `[providers.<name>]`
block, so one key can back many model profiles; inline profile fields override the shared block.

Fields:

- `model`: required model identifier (no default).
- `context_window`: optional integer used for context-percentage reporting.
- `[provider].base_url` / `[profiles.*].base_url`: OpenAI-compatible base URL, defaults to
  `https://api.openai.com/v1`.
- `[provider].api_key` / `[profiles.*].api_key`: required; allowed only in the global file. A
  project file that sets it anywhere is rejected, so a committed project config can never leak a key.
- `default_profile`: chooses the starting profile when several are defined; a project file may set
  it to pin a project to a profile.
- `tools` (under `[provider]` or `[profiles.*]`): whether to offer tools to that model, default
  true. Set false for endpoints/models that reject the `tools` field (many small local models) so
  plain chat still works; `/model` marks such profiles `[no tools]`.
- `[permissions].allow_commands`: exact-match `run_command` commands that skip approval, global-only
  like `api_key` — a project file that defines `[permissions]` at all is rejected, since an
  allowlisted command grants unattended execution the same way a leaked key would.
- `[commands].timeout_secs` / `[commands].background_max_secs`: `run_command`'s foreground timeout
  (default 30) and the safety cap on a `background: true` job's lifetime (default 1800). Not
  security-relevant, so unlike `[permissions]` a project file may override either.
- `[pricing]`: optional cost reporting. `[pricing.models."<model>"]` takes `input_per_million` and
  `output_per_million` (two separate rates; both are required, since half a price is not a price),
  quoted per million tokens because that is the unit provider pricing pages use. `[pricing].currency`
  is a display symbol only — Kamui never converts currencies. Model ids are matched exactly, ignoring
  case and surrounding space, with no wildcards. Not security-relevant, so (like `[commands]`) a
  project file may set prices; they merge into the global table per model. Absent `[pricing]`,
  `Config::prices` is empty and cost never appears anywhere.
- `[provider].embedding_model` / `[profiles.*].embedding_model`: an embedding-capable model on the
  same provider, enabling `/index` and `search_code`. Inherits from a shared `[providers.*]` block
  the same way `base_url`/`api_key`/`tools` do. `None` (the default) leaves semantic search
  unavailable — `search_code` is then not offered to the model at all, rather than erroring.

On first run, when no global `kamui.toml` exists, Kamui scaffolds the global config directory with a
commented template and exits, asking the user to fill in the key. `KAMUI_DATA_DIR` remains an
environment override for the database location only (a container/ops concern, not provider config).

Never commit API keys, credentials, provider responses containing secrets, or local database files.
A project `kamui.toml` is safe to commit because it cannot contain a key. If a key appears in logs,
chat, commits, or screenshots, advise immediate rotation.

OpenRouter, Ollama, LM Studio, Groq, DeepSeek, and LiteLLM work through an OpenAI-compatible base
URL by editing `provider.base_url` and `model` in `kamui.toml`; README documents OpenRouter and
Ollama examples. Describe these as OpenAI-compatible services, not native provider integrations.
Native Anthropic and Gemini support would require dedicated adapters and is not currently planned.

## RTK Decision

[RTK](https://github.com/rtk-ai/rtk) is an optional external execution backend, now wired into
`run_command`: it is detected once per process and simple commands are prefixed with `rtk`, while
anything with shell operators, an existing `rtk` prefix, a shell builtin or `VAR=value` prefix as
its first word, or a system without RTK runs directly.
It is a Rust application, but it currently exposes a binary target rather than a stable public Rust
library API. Do not add it as a Cargo dependency or copy its source into Kamui.

The intended execution flow is:

```text
model requests a command
  -> Kamui validates permission and policy
  -> Kamui applies timeout, cancellation, and output limits
  -> supported command runs through the external `rtk` binary when available
  -> otherwise command runs directly
  -> Kamui records the command, exit status, and result
  -> compact output enters model context
```

RTK responsibilities:

- Filter, group, deduplicate, and compress command output.
- Preserve useful failures and command exit status.
- Reduce model context used by tests, builds, searches, Git, containers, and other supported tools.

Kamui responsibilities that RTK does not replace:

- Tool-call protocol.
- User permission and confirmation policy.
- Path and command safety.
- Timeout and cancellation.
- Output size limits.
- Audit trail and recovery behavior.
- Direct-command fallback.

Do not require RTK for normal chat or repository context. Detect it at runtime; `kamui doctor`
reports its availability and direct command execution remains the fallback.

## Priorities

The source of truth is `ROADMAP.md`. Current priority order is:

1. Phase 2 is complete. Custom global instructions and Markdown export were descoped; do not start
   them without a concrete user request.
2. Phase 3 is complete and has grown past its original scope: the provider-agnostic tool-call
   protocol, streaming agent loop, read/search tools (`read_file`/`list_directory`/`grep`/`glob`),
   the confirmation-gated command runner (configurable timeout, optional RTK routing, a global
   `[permissions]` allowlist, `--auto-approve`, and `background: true` jobs with
   `command_status`/`stop_command`/`/jobs`), the confirmation-gated `patch_file` editor with a
   turn-scoped snapshot/auto-revert safety net and `/undo`, `update_plan` for a live checklist,
   `spawn_agent` for concurrent batches of up to four read-only sub-agents, whole-turn persistence
   including tool messages, per-turn usage accounting, and interrupt-and-continue cancellation. Multi-file
   editing is repeated `patch_file` calls within a turn; Git works through `run_command` plus
   `@diff`/`@staged`; the audit trail is the persisted tool messages.
3. Phase 5 is complete for its planned scope (config, runtime `/model` switching with profiles and
   shared credentials, OpenAI-compatible docs, per-model stats). An agentic system prompt
   (`src/prompt.rs`) now ships on every request.
4. Phase 4 context management: image input, directory context, and context compaction are done.
   Semantic search (`/index`/`search_code`) now uses boundary-aware chunking, batched transactional
   indexing, embedding-model tracking, and hybrid FTS5/LSH candidate retrieval with exact cosine
   reranking. Excel/PDF input are handled via MCP; Anthropic/Gemini native providers are not planned.
5. Phase 6 has a line-oriented rich terminal feed and repeatable JSON benchmark mode. Persistent
   one-shot/interval command scheduling ships through `kamui jobs` and its foreground worker.

Avoid starting these early because their true scope is large:

- Further context compression beyond the rolling-summary compaction already shipped.
- Plugin systems or remote workers. Persistent local scheduling and concurrent read-only sub-agents
  are already done; remote execution remains a separate trust and transport problem.
- GUI, mobile, and voice clients. This includes Kamui Dispatch (a phone-to-host remote-control
  system, see ROADMAP.md's "Later" section for the planned architecture) — it is a separate
  project (a Flutter app, a small relay backend, and a host-side dispatch agent binary), not a
  change to Kamui itself. Kamui already exposes the exact interface a dispatcher needs (`-p
  <prompt>`, denying unattended mutation by default); do not add HTTP server, daemon, or
  network-facing code to the Kamui binary for this.

## Coding Principles

- Prefer the smallest correct change.
- Keep behavior cross-platform across Windows, Linux, and macOS.
- Treat filesystem boundaries, symlinks, command invocation, and subprocess output as hostile input.
- Preserve existing user data and shipped behavior when changing storage or sessions.
- Avoid backward-compatibility layers unless persisted data, released behavior, or external consumers
  require them.
- Add concise comments only where behavior is not self-explanatory.
- Keep user-facing command names consistent. Resume uses `/resume` and `-r`; do not introduce
  ambiguous aliases without a concrete need.
- Do not persist expanded repository context unless a future design explicitly requires it.
- Do not count title-generation calls as primary chat requests.
- Do not save a partially streamed exchange as if it completed successfully.

## Verification

Before considering Rust changes complete, run:

```sh
rtk cargo fmt --all
rtk cargo test
rtk cargo clippy --all-targets --all-features -- -D warnings
rtk git diff --check
```

If RTK is unavailable, run the same commands without the `rtk` prefix. Also run `cargo check` or a
release build when changing dependencies, platform behavior, installers, or release packaging.

Current tests cover persistence, cascade deletion, session summaries, hidden empty sessions, SSE
parsing, project instruction precedence, file-reference expansion, duplicate references, unchanged
plain prompts, and staged Git diff expansion. Add focused tests for new parsing, storage, safety, and
cross-platform path behavior.

## Git and Releases

- Do not commit or push unless the user explicitly requests it.
- Do not rewrite or move a published tag. If release code changes after a tag, create a new patch
  version and tag.
- Release tags matching `v*` trigger five build targets: Windows x64, Linux x64, Linux ARM64, macOS
  Intel, and macOS Apple Silicon.
- GitHub Release assets are required before public installers work because installers download from
  `releases/latest/download` and verify checksums.
- Tag `v0.1.0` points to the initial persistent streaming chat release commit. Its workflow run was
  blocked before jobs started because the GitHub account was locked due to a billing issue. No empty
  GitHub Release was intentionally published. Re-run or create the next patch release only after
  GitHub Actions billing is operational.

## Known Limitations

- The current provider uses the Chat Completions API, not a native Responses API or native tool-call
  loop.
- Project instructions are loaded only from the launch directory, not recursively from ancestors or
  nested directories.
- `@diff` excludes untracked files and `@diff`/`@staged` require Git on `PATH`.
- Context limits are byte-based rather than tokenizer-aware.
- Cost reporting knows only the prices the user configured, matched against the exact model id
  recorded on each usage row. Kamui bundles no price list (it would go stale) and infers nothing
  per provider, so a model that has not been priced is reported as `unpriced` rather than costed.
- Currency is a label, not a unit: prices from providers billing in different currencies would be
  summed as if they were the same one.
- Unix installer behavior has not been exercised locally from the Windows development environment.

## Definition of Done

A feature is complete when:

- Its behavior is provider-neutral unless explicitly provider-specific.
- Failure modes are clear and do not corrupt sessions or files.
- Relevant unit tests exist.
- Formatting, tests, strict Clippy, and diff checks pass.
- User-facing behavior is documented in `README.md`.
- Product priority or completion state is reflected in `ROADMAP.md`.
- No secret, local `.env`, database, build artifact, or unrelated worktree change is included.
