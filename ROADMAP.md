# Kamui Roadmap

Kamui is evolving from a provider-agnostic chat CLI into a repository-aware coding agent. The
roadmap prioritizes a safe, useful read-only workflow before file mutation and command execution.

Status: Phases 1–5 are complete for their planned scope, and the MCP client has shipped. Kamui is a
working coding agent — it explores, reads, runs commands (with approval and optional RTK
compression), and edits files (with a diff preview and approval), persisting whole turns and letting
the user interrupt with `Ctrl+C`. Long sessions compact themselves into a rolling summary. It is
configured through `kamui.toml`, `/model` switches between named provider profiles at runtime, and
MCP servers can contribute their own tools.

The repository-aware runtime, scalable local search, benchmark runner, line-oriented terminal UI,
concurrent read-only sub-agents, and persistent scheduled command queue have shipped. Image tools
(`read_image` plus `@image` / `@clipboard`) are in tree. The next priority is not more surface area
— it is the dogfooding UX friction below, then the Phase 8+ items.

## Phase 1: Chat Foundation

- [x] Interactive streaming chat
- [x] Provider-agnostic core
- [x] OpenAI-compatible provider
- [x] SQLite session storage
- [x] Session list, resume, and delete
- [x] Auto title generation
- [x] Token and context usage statistics
- [x] Cross-platform installers and release workflow

## Phase 2: Repository-Aware Chat

- [x] Project instructions (`KAMUI.md` or `AGENTS.md`)
- [x] Read-only `@file` context
- [x] Git diff context (`@diff` and `@staged`)
- [x] Session rename
- [x] Conversation search
- [x] Latency and time-to-first-token tracking

Descoped from Phase 2 (not planned): custom global instructions and Markdown export. Do not
re-add without a concrete user request.

## Phase 3: Coding Agent Runtime

- [x] Provider-agnostic tool-call protocol
- [x] Tool runtime, dispatch, and streaming agent loop
- [x] Read file tool
- [x] List directory tool
- [x] Safe terminal command runner with permission, timeout, and output limits
- [x] Background command execution (`run_command(background: true)`, `command_status`,
  `stop_command`, `/jobs`)
- [x] Preserve raw output on failures and command exit codes
- [x] Optional RTK execution backend with direct-command fallback
- [x] Patch file tool with confirmation
- [x] Multi-file editing
- [x] Git status, diff, and commit integration
- [x] Tool audit trail
- [x] Cancellation and failed-tool recovery

Phase 3 is complete. Kamui can explore (`list_directory`), read (`read_file`), execute
(`run_command` with approval and optional RTK compression), and edit (`patch_file` with preview and
approval), with whole turns persisted so resumed sessions replay tool interactions. A turn's
recorded usage folds every agent-loop round together: output tokens accumulate across rounds while
the input count tracks the final round, so context reporting stays correct without double-counting.

How the last four items are satisfied:

- Multi-file editing: the agent loop runs any number of `patch_file` calls within a single turn,
  each independently previewed and approved, so a change spanning several files is one conversation
  turn. A single atomic multi-file transaction is deliberately not built; each file is its own
  reviewable, atomic write.
- Git status, diff, and commit integration: Git is available through `run_command` (with approval
  and RTK compression) for status, diff, and commit, and read-only history context is available
  through `@diff` and `@staged`. No Git-specific tool is needed on top of these.
- Tool audit trail: every tool request and result is persisted as part of the turn (see the tool
  message persistence above) and replayed on resume, which is the durable record of what ran.
- Cancellation and failed-tool recovery: `Ctrl+C` during a turn interrupts it and returns to the
  prompt, killing any running command, without saving the partial turn; at the idle prompt it exits.
  Tool failures (bad arguments, a patch that does not match, a declined command, an unknown tool)
  are returned to the model as text so it can recover on the next round rather than aborting.

Progress: the provider-agnostic tool-call types (`ToolDefinition`, `ToolCall`, tool-request and
tool-result messages), their OpenAI serialization, and both non-streaming and streaming (index-keyed
delta reassembly) parsing have landed with tests. The core no longer serializes its own message
types into an OpenAI-shaped payload; wire mapping lives in the provider. A `ToolRegistry` dispatches
calls, read-only `read_file` and `list_directory` tools reuse the shared `@file` path-safety checks,
and the chat loop runs a streaming agent loop (bounded by a per-turn round limit) that executes
requested tools and feeds results back until the model returns a plain answer. Tool failures are
returned to the model as text so it can recover.

The `run_command` tool executes shell commands in the project directory. Kamui owns the permission
policy: any tool that reports `requires_confirmation` (`run_command` and `patch_file`) is shown to
the user and must be approved with `y`/`yes` before it runs, unless it matches the global
`[permissions] allow_commands` exact-match allowlist or `--auto-approve` is active; declining feeds
a refusal back to the model. Commands run with stdin disabled, a configurable timeout that kills the
process (`[commands].timeout_secs`, default 30 seconds), and a 16 KiB output cap; the result carries
the exit code plus captured stdout and stderr.

`run_command(background: true)` starts a job without waiting: it returns a job id immediately, a
`tokio::task` drains the process independently (so output survives a kill/timeout), and
`command_status`/`stop_command`/`/jobs` check on or stop it. Jobs are in-memory only, bounded by
`[commands].background_max_secs` (default 30 minutes) as a runaway-process backstop, and are killed
on shutdown so nothing outlives the Kamui process.

RTK routing is in place: the `rtk` binary is detected once per process, and simple commands are
prefixed with `rtk` so compressed output reaches model context. Commands containing shell operators,
commands already prefixed with `rtk`, and every command on systems without RTK run directly — as
does a command whose first word the shell handles itself rather than executing, since `rtk` can only
prefix a program. That covers `cmd`/`sh` builtins (listed per shell, because `echo` is a `cmd`
builtin on Windows but a real `/bin/echo` on Unix) and a leading `VAR=value` assignment. The first
line of each result records the exact command line that was executed.

The `patch_file` tool edits one file per call by exact-match replacement: `old_text` must match the
file exactly once, or the patch is rejected with guidance so the model re-reads and retries; an
empty `old_text` creates a new file that must not already exist. Every patch shows a +/- line
preview and requires the same `y`/`yes` approval as commands. Writes go through a temporary file
and rename so an interrupted write cannot leave a half-written file, and paths resolve through the
same containment checks as reading (a new file's parent directory must already exist inside the
project). Multi-file editing beyond one file per call remains future work.

Tool messages are persisted. A `user_version = 3` migration rebuilds the `messages` table to allow
the `'tool'` role and store `tool_calls` and `tool_call_id`, and a whole turn (prompt, tool requests,
tool results, final answer) is saved atomically, so resumed sessions replay the tool interactions.
Per-turn usage now folds every agent-loop round together (output summed, input from the final round).
`Ctrl+C` interrupts a turn and returns to the prompt without saving it, killing any running command.

RTK is an execution optimization, not the command permission layer. When installed, Kamui should
route supported terminal commands through the external `rtk` binary to reduce tool output before it
enters model context. Unsupported commands and systems without RTK must continue to work directly.
Raw `git diff` remains available for code review because a condensed diff can omit required detail.

## Phase 4: Context Management

- [x] Directory context with ignore rules
- [x] Clipboard context (`@clipboard`, text or image)
- [x] Conversation summarization
- [x] Context compression
- [x] Project indexing (a deliberately simple v1 — see below; a larger-scale version remains open)
- [x] Semantic search (`/index`, `search_code`)
- [x] Image input
- [x] `read_image` tool (agent-driven; same decode path as `@image`)
- [ ] PDF input (not planned — handled via MCP)

Directory context is built. Referencing a directory (`@src`) attaches the text files inside it,
walking with the `ignore` crate so `.gitignore`, global excludes, and hidden files are honoured even
outside a git repository. Files are attached in path order until the shared 128 KiB context budget
or a 50-file cap runs out; anything left over (binary, too large, or over budget) is reported in a
trailing note instead of failing the prompt.

Image input is built, from a file, the clipboard, or the `read_image` tool. Referencing an image
(`@shot.png`, also `.jpg`/`.jpeg`/`.gif`/`.webp`) attaches it to the request instead of inlining
bytes as text: the file is read through the same containment checks, capped at 5 MiB, and carried as
a base64 `ImageAttachment` on the message. `@clipboard` prefers text but falls back to clipboard
image data, encoding it as PNG, so a screenshot can be pasted straight into a prompt — terminals
cannot accept pasted image data directly, so this is the paste path. `read_image` uses the same
decode path when the model asks for a project screenshot mid-turn; vision attachments are appended
only after every contiguous `tool` result in that batch so OpenAI-compatible upstreams do not 400
on unpaired `call_id`s. The OpenAI adapter emits a content-parts array (`text` plus `image_url`
data URLs) only when images are present, so text-only requests keep their plain-string content.
Images are per-request and are not persisted. The model must support vision.

Context compaction is built (`src/compaction.rs`). When a session's un-summarized recent history
grows past a byte threshold (about half the profile's `context_window`, or a default), the older
messages are folded into a rolling summary via one non-streaming request, and the request thereafter
sends the agentic prompt plus that summary plus the most recent messages verbatim. `/compact`
triggers it manually. The full history stays in storage; only the per-request message list is
compressed, and the summary is in-memory (regenerated after a resume). Excel/PDF input are handled
through MCP.

Semantic search started as a deliberately simple v1 and now includes a scalable local retrieval
layer. `Provider` exposes an `embed(model, texts) -> Vec<Vec<f32>>` method
(`src/provider/openai.rs`, via the OpenAI-compatible `/embeddings` endpoint), and a profile can set
`embedding_model` to enable it. `/index` (`chat::run_index`) walks the project the same
`.gitignore`-aware way `grep`/`glob` do, splits each file into roughly 50-line chunks with
lightweight boundary heuristics (`tools::chunk_text` prefers blank lines and common declaration
starts near the target), and skips any file whose content hash
(`chat::content_hash`) matches what `indexed_files` recorded last run, so a second `/index` after a
small edit only re-embeds what changed. Chunks and their embedding vectors (little-endian `f32`
bytes in a BLOB column) live in `code_chunks`. Both tables are keyed by a `project` column
(`user_version = 8`, the canonical project root from `ProjectContext::key`), so several projects can
share the one global database without their identically-named relative paths colliding.
Schema v10 adds an FTS5 mirror and an LSH bucket per embedding. `search_code` embeds the query,
unions lexical/path and nearby-vector candidates, and exact-cosine reranks that bounded shortlist,
returning the top 8 as `path:start-end` with the chunk text. Projects with at most 2,000 chunks keep
exhaustive ranking. Without `embedding_model` configured, `search_code` is not offered to the model.

Maintaining the index is automatic; building it is not. After a persisted turn,
`chat::refresh_index_for_paths` re-embeds every `patch_file` target already present in the index
(sharing `chat::index_file` with `/index`), so `search_code` cannot quote code that no longer
exists at the lines it reports. Files the turn *created* are deliberately left out: stale content
is misleading, a missing file is only incomplete, and only the former is worth spending the user's
embedding budget without being asked.

Building the index stays manual by design. Cursor-style automatic indexing on open does not fit
Kamui: it is a start-and-exit CLI rather than a long-lived editor, so background work has nowhere
to hide, and embedding runs against the user's own API key — silent spend at startup is the wrong
default. What
ships instead is a staleness hint (`chat::index_staleness`): when a project has been indexed before,
the startup banner reports how many files changed, appeared, or disappeared, and leaves the decision
to run `/index` to the user. It compares mtimes against `indexed_files.indexed_at` rather than
re-hashing content, so it costs one directory walk and no network calls; that makes it advisory,
which is acceptable because `/index` still does the authoritative hash comparison. Interactive chat
only — `-p` output is script input.

Search now scales beyond the v1 full scan. Schema v10 adds SQLite FTS5 rows and LSH buckets;
`search_code` unions lexical and nearby embedding candidates before exact cosine reranking, while
small indexes still use exhaustive ranking. The index records the embedding model, so changing it
forces a rebuild even when source hashes are unchanged. File replacement is transactional and
embedding calls are batched. Chunk boundaries remain heuristic rather than parser-backed.

## Phase 5: Providers and Models

- [x] Structured configuration file (`kamui.toml`) for provider, model, and settings
- [x] Runtime model switching
- [x] Document OpenAI-compatible services (OpenRouter, Ollama, LM Studio, Groq, DeepSeek, LiteLLM)
- [x] Provider and model statistics
- [ ] Temperature and model parameters (not planned — coding agents keep defaults simple; Codex and
  Claude Code do not expose these either)
- [ ] Provider and model discovery (not planned — profiles are defined manually in `kamui.toml`)
- [ ] Native Anthropic provider (not planned)
- [ ] Native Gemini provider (not planned)

Phase 5 is complete for the planned scope: configuration, runtime model switching with profiles and
shared credentials, OpenAI-compatible provider documentation, and per-model usage statistics all
shipped. A `user_version = 5` migration records the model on each usage row, and `/stats` shows a
per-model breakdown when a session used more than one model. The remaining items are descoped.

Configuration is built (`src/config.rs`): a TOML `kamui.toml` replaces `.env` and `dotenvy`. A
global `kamui.toml` in the OS config directory may hold the API key; a per-project `kamui.toml` in
the working directory is non-secret only and is rejected if it sets `api_key`. Resolution precedence
is project, then global, then built-in defaults, with no environment variables in provider or model
configuration. First run scaffolds the global config directory with a commented template and exits
so the user can fill in the key. `KAMUI_DATA_DIR` still overrides only the database location.

Runtime model switching is built. The config accepts named `[profiles.<name>]` entries; the flat
single-provider form remains valid as one implicit profile. Several profiles can share one key by
referencing a `[providers.<name>]` block. A per-profile `tools` flag (default true) turns tools off
for models that reject the `tools` field — many small local models — so plain chat still works; those
profiles are shown as `[no tools]`. In chat, `/model` lists profiles and `/model <name>` switches the
active provider and model, persisting the choice in the SQLite `settings` table so it survives
restarts. The provider is rebuilt through a factory injected by `main`, so the chat loop stays
provider-neutral.

Thinking spinner and interrupt-and-continue (Phase 6 items) also shipped early: a braille spinner
animates until the first token, and `Ctrl+C` mid-turn returns to the prompt instead of exiting.

Orvix Coding Plan support extends Phase 5: `completions_path` plus a sticky `session_id`
(`send_session_id`) route a session to one upstream worker, and the **Coding cache contract**
(`src/cache.rs`, documented in `CLAUDE.md`) keeps the request prefix byte-stable so that worker's
prompt cache actually holds. Memory and the rolling summary moved out of the system message into a
volatile tail behind the history; `PrefixGuard` reports any prefix that moves; compaction waits
until ~85% of the context window on a pinned profile; and `/stats` reports the per-session hit
median, the share of turns at or above 90%/95%, and how many turns were warm-ups. Title generation
and compaction use derived sticky ids (`{session}:title` / `{session}:compact`) so side requests
cannot evict the conversation's warm prefix. The fullscreen Context rail and token badge surface
per-turn cache (including warm-up zeros on pinned profiles) plus the session median when measured.
Streaming and non-streaming calls share the same bounded pre-output retry policy, and an identical
tool batch requested for three consecutive rounds is stopped before its third execution.

## Phase 6: Terminal Experience

- [x] Project status card and `/status` refresh
- [x] Markdown rendering (`src/markdown.rs`)
- [ ] Syntax highlighting (not planned for now — see below)
- [x] Thinking indicator (braille spinner until the first token)
- [x] Rich terminal UI (line-oriented event feed)
- [ ] Session pinning (not planned — see below)
- [x] Prompt templates and system prompt profiles (one feature: user-defined `/commands`)
- [x] Daily and monthly usage reports (`/usage`)
- [x] Optional cost tracking (`[pricing]` in `kamui.toml`)
- [x] Benchmark mode

Markdown rendering (`src/markdown.rs`) styles streamed output a line at a time: deltas accumulate
in a buffer and a line is rendered only once its newline arrives, which keeps streaming visibly
live while letting each line be styled as a whole. That works because nearly every construct worth
rendering (headings, fences, list markers, blockquotes) is line-scoped, and the inline ones (bold,
code spans) practically never straddle a line break. The buffer is flushed both at stream end and
on `Ctrl+C`, so an interrupt never swallows the partial line already received.

The supported subset is deliberately narrow, because mangling code is worse than under-styling
prose: `_italic_` is unsupported (it would corrupt `snake_case` identifiers) and so is single-`*`
italic (it would corrupt globs like `src/*.rs`). Bold requires non-padded content, the standard
rule that also keeps `src/**/*.rs` intact. Code spans are never reinterpreted, so a backticked
`**literal**` stays literal. Styling is skipped when stdout is not a terminal, and when `NO_COLOR`
is set. Syntax highlighting inside fenced blocks is a much larger job (a grammar per language, or
a heavy dependency like `syntect`) for a marginal gain over the current single-colour blocks; it is
not planned unless it turns out to matter.

Prompt templates and system prompt profiles turned out to be the same feature, and shipped as
user-defined commands (`src/commands.rs`): a markdown file in `<project>/.kamui/commands/` or
`<config dir>/kamui/commands/` becomes `/<name>`, expanding into that turn's prompt. Project
commands shadow global ones by name, matching `kamui.toml` precedence — which maps directly onto
the real-world split that motivated the feature, where a prompt library has both general-purpose
agents (a code reviewer, a test engineer) and ones encoding a single codebase's conventions. It is
text substitution and nothing more: no new capability, no approval bypass, so the risk matches the
`KAMUI.md` project instructions that already ship. Chaining between commands is deliberately absent
— the pipeline in a prompt library is a recommended reading order, not something the tool needs to
model, and each command stands alone.

Usage reporting split cleanly from the existing `/stats`: that stays scoped to the active session,
while `/usage` aggregates every session by day and by month over the `usage_records` rows already
being written (no migration needed). Both count requests as `kind = 'chat'` only while summing
tokens across all kinds, so title generation shows up in spend but not in request counts.

The rich terminal increment deliberately stays line-oriented rather than taking over the alternate
screen: tool calls report lifecycle state, duration, and output size; the spinner only runs for a
real interactive terminal; piped and `-p` output stays plain and deterministic. `NO_COLOR` disables
colour without removing the structured event feed.

`kamui benchmark <suite.json>` runs repeatable prompt cases against a chosen profile, supports
multiple runs, checks optional case-insensitive expected substrings, reports latency and token
totals, and exits non-zero on failed expectations.
For Orvix Coding profiles, repeated runs are append-only turns in one stable session per case and
the final report includes median/aggregate cache rates plus the shares at or above 90% and 95%.

Cost tracking needed no migration either, and no new writes: `usage_records` has carried
`input_tokens`, `output_tokens`, and the `model` that produced them since `user_version = 5`. The
only missing ingredient was prices, which only the user can supply. A `[pricing.models."<model>"]`
entry in `kamui.toml` (`src/pricing.rs`) takes `input_per_million` and `output_per_million` — two
rates, never one, because every provider charges more for output and a blended rate would misreport
every session, and per *million* tokens because that is the unit the provider's own pricing page
uses, so the number is copied across rather than converted. Kamui does not know or convert exchange
rates: amounts are in whatever currency the prices were typed in, and `[pricing].currency` only
changes the symbol printed beside them. Prices are not secret and grant no capability, so — unlike
`[permissions]` — a project `kamui.toml` may set them, merged into the global table per model.

Optional means optional. With no `[pricing]` section, `/stats` and `/usage` print exactly what they
printed before, down to the column widths: no cost column, no zero rows, and not even the extra
per-model query. When prices *are* configured, the case worth getting right is a model that has
usage but no price: it reads `unpriced` rather than a number, and any total that includes such
usage carries a trailing `+` and a note underneath, because an amount that silently omits part of
the spend is a lie about money. The accepted limitation is that a price applies to the exact model
id it names (matched case-insensitively, but with no wildcards and no per-provider defaults). That
also means Kamui ships no built-in price list to go stale — a model the user has not priced simply
reports as unpriced until they do.

Session pinning was considered and dropped. Kamui sessions are task-scoped — opened for a job,
finished, rarely revisited — so the case for pinning is weak, and `/search` already covers the
real need (finding an old session by what was discussed, which is what you actually remember).
Revisit only if a concrete "I return to these exact sessions daily" case appears.

## Later: Extensibility and Remote Work

- [x] MCP client
- [ ] Plugin API and manager (not planned — see "Plugin decision" below)
- [x] Background jobs and persistent command queue
- [x] Scheduled tasks
- [ ] Worker nodes and remote execution
- [ ] Kamui Dispatch: remote prompt dispatch from a phone (planned architecture, not started —
  see below; distinct from "Worker nodes," which is about distributing compute, not remote control)
- [x] Local memory (`remember`/`update_memory`/`forget`, `/memory`, `/forget`)
- [ ] RAG beyond the shipped semantic search (a user-supplied corpus, and retrieval the model does
  not have to ask for — see below)
- [x] Multi-agent workflows (a first, deliberately narrow form: `spawn_agent`)

The MCP client is built (`src/mcp.rs`, via the `rmcp` SDK). Servers declared as `[mcp.<name>]` in the
global `kamui.toml` are launched as child processes over stdio; each tool they advertise is wrapped
as a Kamui `Tool` with a `<server>__<tool>` name, so MCP tools flow through the same registry,
permission policy, and agent loop as the built-ins. Every MCP call asks for approval unless the
server is marked `trusted`. Project files may not declare servers, because launching one is arbitrary
code execution. A server that fails to start is reported and skipped rather than blocking startup.
Only stdio transport and the tools capability are supported by design.

**Plugin decision**: a dedicated plugin runtime was considered and rejected. Kamui is already an
MCP *client*, so "write a plugin" already means "write (or reuse) an MCP server" — the gap is
packaging/discovery (no `kamui plugin install <name>`), not the extension mechanism. A realistic
native plugin API would mean either unsafe/version-fragile dylib loading, or a WASM sandbox that
converges back on "an MCP server, in our own protocol." A separate plugin runtime remains out of
scope.

Local memory shipped (`user_version = 6`). `remember`, `update_memory`, and `forget` let the model
keep a small set of facts; `/memory` and `/forget <text>` manage the same store directly, without
going through the model. It is global and permanent on purpose: unlike `KAMUI.md` (a file, per
project) or session history (one conversation), a remembered fact follows the user into every
project afterward, so the whole store — capped at 4 KiB — is read fresh from SQLite into every
turn's system prompt.

The RAG half of that entry is only partly answered, so it stays open, split out above. Phase 4's
`/index`/`search_code` is retrieval-augmented generation in substance: chunks are embedded, stored
locally in the same SQLite database, and the closest matches are fed back into the turn. Two things
it is not. Retrieval is over the project's own source files, walked the same `.gitignore`-aware way
`grep` is — not over a corpus of documents the user brings. And nothing is retrieved unless the
model decides to call `search_code`; there is no automatic retrieval step on every turn, for the
same reason indexing is not automatic (it would spend the user's embedding budget unasked, on turns
that never needed it).

`spawn_agent` (`src/tools.rs`/`chat::run_spawned_agent`) is Kamui's first multi-agent primitive: it
delegates a self-contained task to an isolated sub-agent — fresh system prompt, no shared history
with the parent — and returns only the sub-agent's final answer, so its own tool-call trace never
enters the parent's context. Deliberately narrow for v1: the sub-agent runs against
`ToolRegistry::read_only` (`read_file`/`list_directory`/`grep`/`glob` only, no `run_command`,
`patch_file`, memory tools, or recursive `spawn_agent`), so none of its calls ever need
confirmation and there is no approval flow to reproduce. Several independent `spawn_agent` calls
in one model response run concurrently in batches of at most four; results are fed back in original
tool-call order. The read-only boundary is what makes concurrency safe without parallel approvals.

Persistent scheduled commands live in `scheduled_jobs` (schema v9). `kamui jobs add` creates
one-shot or interval jobs, `list`/`cancel`/`pause`/`resume` manage them, and `kamui jobs worker`
claims due work atomically. Queue state, capped output, and exit status survive restarts. The worker
is intentionally foreground; OS service installation is not hidden inside Kamui, and missed
intervals coalesce into one future run rather than backfilling a burst.

**Kamui Dispatch (planned architecture, not started)**: lets a phone trigger Kamui running on a
host machine — "prompt your coding agent from your pocket." Two shapes were weighed: a VPN-mesh
alternative (Tailscale/WireGuard between phone and host, plus a new local `kamui serve` HTTP mode —
no backend at all, simpler and more private, but every device needs the VPN client installed first)
versus a relayed backend, chosen because it works from an arbitrary phone/network without asking
the user to set up a mesh VPN first.

Three separate pieces of software, none of them living inside the Kamui binary — the same boundary
Kumo already draws for itself ("Kamui remains an independent coding agent and does not need to know
about Telegram"):

- **Flutter app** (phone): pairing UI, prompt input, response view. Talks to the backend over
  ordinary HTTPS — nothing unusual here, since the backend has a public address.
- **Backend** (new, a small relay service): a pairing endpoint (one-time code/QR, the same shape
  Kumo's Telegram pairing already uses), a submit-prompt endpoint for the phone, and a
  fetch-pending-work endpoint for the host to poll. It queues one prompt at a time per paired host
  and relays the result back; it does not need to store conversation history itself — Kamui's own
  SQLite already does that on the host.
- **Dispatch agent** (new, a separate small binary on the host machine — not Kamui itself): holds
  the outbound connection to the backend and invokes `kamui -p <prompt>` when a prompt arrives,
  exactly the way Kumo already delegates to Kamui today via `delegate_to_kamui`. Kamui needs *zero*
  code changes for this: `-p` (see "Current Product Behavior" in `CLAUDE.md`) is already the exact
  interface a remote dispatcher needs.

Why the backend-to-host leg can't be plain HTTP: the host machine (a laptop or desktop) almost never
has a public IP or an open inbound port, so the backend cannot connect to it — the connection has to
be initiated by the host instead. Either long polling (the host repeatedly asks "anything for me?",
simple, no special infrastructure) or a persistent WebSocket (lower latency, more moving parts:
reconnect and heartbeat logic). Long polling is the recommended v1; a WebSocket upgrade is a later,
independent improvement if latency turns out to matter in practice. The phone-to-backend leg has no
such constraint — the backend is always reachable, so it's ordinary REST.

The open safety question matters more than the transport choice: the dispatch agent's natural call
is `kamui -p <prompt>`, and Kamui's own non-interactive default is to *deny* any tool that would
need confirmation (`run_command`, `patch_file`) rather than silently allow it (see "Current Product
Behavior") — the dispatcher does not need `--auto-approve` to be useful, and should not default to
it. A stolen phone or leaked pairing token that can only read files and answer questions is a much
smaller blast radius than one that can run arbitrary commands unattended. If mutating actions from a
phone turn out to be a real need later, the right shape is very likely a remote approval flow
modeled on Kumo's own Telegram inline-button approval (surface the pending command back through the
backend, wait for a tap, same bounded timeout) — not a blanket `--auto-approve` on every remote
prompt.

## Not Planned Soon

- [ ] GUI and dashboard
- [ ] Mobile app (Kamui itself running natively on a phone — distinct from the Kamui Dispatch
  companion app above, which remotely controls a Kamui that still runs on a host machine)
- [ ] Voice mode
- [ ] MCP server
- [ ] Infrastructure-specific plugins (Docker, Kubernetes, PostgreSQL)

## Phase 7: Fullscreen TUI (stg-az)

- [x] Retained Ratatui transcript with opencode-style thick-border message rails
- [x] Two-tone KAMUI logo home screen with startup notices
- [x] Live editor box: slash menu, placeholder, horizontal scroll, backslash-newline multiline
- [x] InputHub: editor stays live while the agent runs, Enter queues, Esc/Ctrl+C interrupt at
      every await point, approval and ask_user answers bypass the queue via requesters
- [x] Sidebar rail: session title/id, model, project, context pressure, last-turn metrics
- [x] Ctrl+K model picker, Ctrl+S session switcher, `?` keybinding sheet (modal overlays that
      submit real slash commands)
- [x] Permission modal for tool/plan approvals (Allow once / Always / Reject) with preview body
- [x] Word-aware wrapping; PgUp/PgDn/Home/End/Ctrl+U/Ctrl+D scrolling; mouse-wheel scroll
- [x] Model registry wizard reusing onboarding (base URL + key -> list_models -> pick -> profile)
- [x] Skill loader leniency matching opencode plus /warnings details and agent-driven repair
- [x] Safe exit: double Ctrl+C confirmation at idle prompt

Deferred from Phase 7 originally: theme switching, @-file completion popup, timeline/subagent
dialogs. The completion popup and Plan Mode/resume friction are now tracked under **Near-term**
below — work those before Phase 8–11 polish.

## Near-term: Dogfooding UX (priority)

Captured from real fullscreen Kamui sessions (2026-09). Happy-path friction beat aspirational
polish: users hit these before they ever need a design system or batch-approval modal.

### Session and commands

- [x] `/resume` with no id opens the session picker (same path as Ctrl+S / `/sessions`)
- [x] Accept unambiguous prefixes such as `/sess` → `/sessions` (and keep “did you mean” for
       ambiguous typos)
- [x] Model picker lists every configured profile, not only the active/default entry

### Plan Mode and live input

- [x] One clear approve path: after the user approves (permission modal or an explicit `approve`),
       do not leave them in “plan not approved — still in Plan Mode” and ask again
- [x] Short control replies typed while a turn is running (`approve`, mode status, stop intents)
       must not silently land in the Enter-queue as ordinary chat; consume them or surface that they
       queued
- [x] Sidebar / notices must agree on mode (`plan` vs `build`) the moment approval lands

### Editor and agent habits

- [x] `@`-file / directory completion popup in the fullscreen editor (Phase 7 deferred item;
       still the highest-value TUI completion gap)
- [x] Prefer built-in `read_file` / `grep` / `glob` over `run_command`+`sed` for reading project
       files (prompt/tool guidance), so dogfooding turns stay short

### Image / multimodal (shipped basis)

- [x] Explicit `read_image` tool alongside `@image` / `@clipboard` context input
- [x] Defer vision `user` attachments until after every contiguous `tool` result in the batch
       (fixes upstream 400 `No tool call found for function call output with call_id …` on
       OpenAI-compatible / Orvix Coding)

### Interactive navigation

- [x] Clickable autocomplete rows, picker rows, approval options, and `ask_user` choices
- [x] Contextual mouse-wheel navigation for overlays, dialogs, autocomplete, and transcript
- [x] Modal click-through prevention and bounded transcript click targets
- [x] Semantic mouse targets for sidebar/footer actions with overlay precedence and hover feedback

### Reliability and recovery

- [x] Durable FIFO input queue with crash recovery and atomic completion alongside turn persistence
- [x] Mutating-tool execution journal with interrupted recovery and bounded `/audit` viewer
- [x] Durable read-only child-agent lifecycle records with bounded `/agents` viewer
- [x] Multi-level session-scoped `/undo` and `/redo`, including migration from one-level snapshots
- [x] Exact command/canonical path session grants instead of tool-wide `always` permissions
- [x] Unique line-trimmed patch fallback while preserving strict ambiguity rejection

## OpenCode-inspired TUI direction

Kamui's fullscreen TUI is deliberately inspired by OpenCode (boxy rails, live editor, sidebar).
Steal **behavior**, not chrome. Do not chase every OpenCode panel before Near-term dogfooding is
usable — users fail on approve / resume / picker / queue long before they need a design system.

### Steal from OpenCode

- [ ] **Live sidebar as single glance**: mode (`plan`/`build`), MCP status (ok/fail + short reason),
      cwd:branch, queued-input indicator, active plan step — not only static session cards
- [ ] **Plan checklist as a first-class UI object**: `update_plan` already exists; render it as a
      clear transcript block *and* a sidebar mirror with the active step highlighted (OpenCode-style
      `# Todos`)
- [x] **Thin turn chrome**: a muted separator before each user turn; keep latency/tokens in the
       sidebar or a small badge — do not compete with the assistant answer
- [x] **Minimal contextual footer**: clickable help, model/session, interrupt, and live-position hints
       state (e.g. commands / interrupt), instead of a crowded always-on shortcut strip

### Do not copy blindly

- Do not add OpenCode-parity panels (LSP list, always-on cost `$0.00`, full MCP inventory) until
  Near-term friction is fixed
- Do not thicken message rails further; the existing opencode-style borders are enough
- Keep Kamui differentiators: permission modal, `--auto-approve`, Plan Mode, Coding cache badge —
  polish their *state transitions*, do not replace them with a chat-app look

### Suggested order after Near-term

1. Live sidebar (mode / MCP / queue / git)
2. Plan checklist mirror + active-step highlight
3. `@` completion + command palette (boxy but fast)
4. Only then Phase 11 visual tokens / themes

## Phase 8: Approval and Multi-Tool Workflow

Do not start this phase until the Near-term dogfooding items above are in a usable state. The core
permission modal already exists; the next step is reducing friction when one turn produces several
independent commands or file edits without weakening reviewability.

- [ ] Group pending tool calls into a single reviewable batch
- [ ] Show a grouped command/diff preview with tool, path, risk, and affected files
- [ ] Approve all, reject all, or approve/reject individual items in a batch
- [ ] Keep approval scope explicit: once, tool-for-session, or reviewed-batch only
- [ ] Preserve path validation, command allowlists, and capability checks for every item
- [ ] Support stop-on-first-error versus continue-independent-items behavior
- [ ] Add batch progress and per-item status in the TUI
- [ ] Add atomic batch rollback while preserving the existing durable multi-level `/undo`/`/redo`
- [ ] Persist an audit record for every requested, approved, rejected, failed, and reverted call

## Phase 9: Context and Agent UX

- [ ] Context inspector showing files, images, tools, and estimated token contribution
- [x] Explicit `read_image` tool alongside existing `@image` context input (see Near-term)
- [ ] Clear model capability indicators for tools, vision, embeddings, and context size
- [ ] Tool-call timeline with duration, output size, cancellation, and sub-agent activity
- [ ] Better test-failure summary with actionable retry and fix flows
- [ ] Theme switching (completion popup tracked under Near-term)
- [ ] Dedicated timeline and sub-agent detail dialogs

## Phase 10: Reliability and Integrations

- [ ] End-to-end coverage for multi-tool turns, batch approval, cancellation, rollback, and resume
- [ ] Provider compatibility tests for text-only and multimodal requests
- [ ] LSP integration for diagnostics, definitions, references, and formatting
- [ ] Extensible plugin/tool contracts with explicit permission boundaries
- [ ] Remote SSH/dev-container execution with visible project and environment indicators

## Phase 11: UI/UX Design System and Interaction Polish

Kamui should feel intentional and polished rather than like a chat log wrapped in borders. Visual hierarchy, interaction feedback, and keyboard behavior must remain clear in both wide and narrow terminals.

### Visual system

- [ ] Define reusable design tokens for color, spacing, borders, emphasis, and semantic states
- [ ] Ship cohesive built-in dark themes with accessible contrast and graceful 16/256-color fallbacks
- [ ] Give user, assistant, tool, warning, error, plan, and approval blocks distinct but restrained styling
- [ ] Improve information hierarchy so primary content stays prominent and metadata remains scannable
- [ ] Keep layout stable while streaming; avoid unnecessary jumps, flicker, and modal size changes
- [ ] Add polished empty, loading, success, error, interrupted, and no-result states

### Batch approval experience

- [ ] Design a dedicated batch-review modal with a summary header and scrollable item list
- [ ] Show each item's tool icon, target, concise action, risk level, and current decision
- [ ] Provide obvious actions: Allow selected, Allow all reviewed, Reject selected, Reject all, and Cancel turn
- [ ] Support keyboard selection, range selection, expand/collapse details, and mouse interaction
- [ ] Preview command output intent and file diffs inline without leaving the approval flow
- [ ] Clearly distinguish `Allow once`, `Allow this reviewed batch`, and `Always allow tool this session`
- [ ] Require additional confirmation for destructive or high-risk commands even inside a batch
- [ ] Show live batch progress and a final per-item result summary with retry options

### Navigation and responsiveness

- [ ] Add a discoverable command palette with fuzzy search, categories, and shortcut hints
- [ ] Add responsive wide, compact, and single-pane layouts based on terminal dimensions
- [ ] Preserve focus predictably when opening/closing modals and return to the previous position
- [ ] Add visible focus, selection, scroll position, and unread/new-output indicators
- [ ] Make transcript, sidebar, context inspector, diff, and tool timeline independently navigable
- [ ] Provide first-run onboarding and contextual hints that disappear after the user learns the flow

### Accessibility and validation

- [ ] Never communicate state using color alone; pair color with labels, symbols, or patterns
- [ ] Respect `NO_COLOR`, terminal capabilities, reduced motion, and configurable animation
- [ ] Document every keyboard shortcut and provide remapping for common conflicts
- [ ] Test at common terminal sizes, including 80x24, split panes, and very narrow windows
- [ ] Add snapshot/golden tests for important screens, dialogs, wrapping, and theme variants
- [ ] Run usability tests for first-time approval, batch review, context inspection, and recovery from errors
- [ ] Establish measurable UX budgets for keypresses, modal depth, visual jitter, and time-to-understand
