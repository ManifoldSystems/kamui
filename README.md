# kamui

Provider-agnostic, repository-aware coding agent for the terminal, written in Rust.

Kamui explores your repository, reads files, runs commands, and edits code, with every side effect
gated behind your approval. Responses stream from any OpenAI-compatible model, `/model` switches
between providers and models mid-session, long conversations compact themselves so they do not
outgrow the context window, and MCP servers can contribute their own tools.

The core request and response types are independent of any single provider's API, so new providers
can be added without changing the chat interface.

## Configuration

Kamui is configured with a `kamui.toml` file. On first run, Kamui starts an interactive onboarding
flow that asks for an OpenAI-compatible base URL and API key, discovers the available models, and
lets you choose the default model. It then saves the configuration and starts the chat immediately:

| Platform | Global config file |
| --- | --- |
| Windows | `%APPDATA%\\kamui\\kamui.toml` |
| Linux | `~/.config/kamui/kamui.toml` |
| macOS | `~/Library/Application Support/kamui/kamui.toml` |

The API key is entered through a hidden prompt. If the global config exists but is missing either a
usable model or API key, Kamui runs the same onboarding flow. Existing advanced multi-profile
configurations are never replaced automatically.

```toml
model = "gpt-5.5"
# context_window = 128000

[provider]
base_url = "https://api.openai.com/v1"
api_key = "sk-xxxxxxxx"
```

You may also place a `kamui.toml` in a project directory to override `model`, `context_window`, and
`provider.base_url` for that project. A project file **must not** contain an `api_key` — Kamui
rejects it, so a project config is safe to commit. The API key lives only in the global file.

Any service implementing the OpenAI Chat Completions API can be used by changing the base URL,
model, and API key. Chat responses use the API's SSE streaming mode and are rendered as deltas
arrive.

**Orvix Coding:** choose the native Orvix Coding option during first-run setup or from the TUI's
"+ Add provider / model" flow. Kamui derives `https://api.orvix.id/v1`,
`completions_path = "/coding/completions"`, and `send_session_id = true`; models are listed from
`GET /coding/models` (the Coding allowlist), not `/v1/models`. The API key is entered in
a masked field and the resulting config is atomically written with owner-only permissions. Use a key with
`coding:invoke`. Kamui sends its session UUID as top-level `session_id` so Orvix can stick the
upstream route for cache. Switch with `/model orvix-coding-flash`. Keep `/v1` profiles for A/B.

On these profiles Kamui also keeps the request prefix byte-stable so that cache can hold: the system
prompt, your project instructions, the skill list and the tool definitions are built once and stay
identical for the whole session, while memory and the rolling summary travel in a tail message just
before your newest turn (still refreshed every turn, just no longer able to reset the cache). Plan
Mode no longer changes the tool list either — it refuses mutating calls when they are attempted
instead. If something does move the prefix — `/model`, toggling a skill, compaction — Kamui says so
instead of leaving you with a silently cold cache, and a turn that misses says which of those is the
likely cause. The per-turn usage line always shows the cache field here, and `/stats` reports how
the session is landing:

```text
Prompt cache:  median 96% over 12 turns | ≥90%: 92% | ≥95%: 75% | warm-up: 1
```

The first turn of a session is excluded from those ratios — there is nothing cached to hit yet — but
a later warm-up turn is counted, because that is a prefix that churned mid-session.

Coding entitlement, quota, concurrency, request-id conflict, authentication, rate-limit, and
temporary server errors are rendered as bounded, actionable messages. Requests have connect and
first-response deadlines; streams have an idle deadline. Before any output is visible, transient
transport errors and HTTP 408/429/502/503/504 are retried up to three attempts using the same body
and session id, respecting a capped `Retry-After`. A stream is never retried after output starts.

### OpenAI-compatible providers

Kamui talks to any OpenAI-compatible endpoint, so you can point it at hosted aggregators or a local
model without a dedicated integration. These are OpenAI-compatible services, not native providers;
tool use and streaming depend on the server and model you pick.

**OpenRouter** (many models behind one key):

```toml
model = "openai/gpt-4o-mini"   # any OpenRouter model id, namespaced as vendor/model
[provider]
base_url = "https://openrouter.ai/api/v1"
api_key = "sk-or-v1-..."
```

**Ollama** (local models, no network key):

```toml
model = "llama3.2"                        # a model you have pulled with `ollama pull`
[provider]
base_url = "http://localhost:11434/v1"
api_key = "ollama"                        # Ollama ignores this, but a value is required
# tools = false                           # set this if the model rejects the tools field
```

**Anthropic (Claude) via OpenAI-compatible proxy** (LiteLLM, `openai-to-anthropic`, etc.):

```toml
model = "claude-sonnet-4-5"               # model id as exposed by your proxy
[provider]
base_url = "http://localhost:4000/v1"     # shim that translates /v1/chat/completions → /v1/messages
api_key = "sk-..."                        # proxy API key
```

Kamui drives Claude through the existing OpenAI-compatible `Provider` — no native Anthropic
adapter. Point `base_url` at a shim that translates `POST /v1/chat/completions` to Anthropic
`POST /v1/messages`. Cache-hit visibility works through the shim: Anthropic
`cache_read_input_tokens` is surfaced via the same dual-path `Usage` deserializer as OpenAI
`prompt_tokens_details.cached_tokens`, so the per-turn usage line and `/stats`/`/usage` show
`cached_tokens` without native code. Prompt-caching `cache_control` and extended thinking remain
native-only and are intentionally deferred (see `docs/research/anthropic-native-adapter-vs-shim.md`).

Many small local models do not support tool calling and reject requests that include tools. If you
see an error like `<model> does not support tools`, add `tools = false` to that profile — Kamui then
chats without offering tools.

Only the global `kamui.toml` holds the key; switching providers is a one-file edit. You can also
keep a per-project `kamui.toml` that sets just `model` and `provider.base_url` to pin a project to a
particular provider (never the key).

### Switching models and providers at runtime

Instead of editing the file every time, define named profiles once and switch with `/model`. Share
one API key across many models by defining a `[providers.<name>]` block and pointing profiles at it
with `provider = "<name>"`:

```toml
default_profile = "sol"

[providers.jatevo]
base_url = "https://api.jatevo.ai/v1"
api_key = "sk-jvo-..."          # defined once, used by every Jatevo profile

[providers.ollama]
base_url = "http://localhost:11434/v1"
api_key = "ollama"

[profiles.sol]
provider = "jatevo"
model = "gpt-5.6-sol"

[profiles.terra]
provider = "jatevo"
model = "gpt-5.6-terra"

[profiles.codeqwen]
provider = "ollama"
model = "codeqwen:latest"
tools = false                   # this model does not support tools
```

Profiles and providers are resolved from the **global** file only. A project `kamui.toml` may set
`default_profile` to pin that project to one of them, but defining `[profiles.*]` or
`[providers.*]` there is rejected outright rather than silently ignored.

In chat, `/model` lists the profiles and marks the active one, and `/model codeqwen` switches the
active provider and model for the next messages. Your choice is remembered across restarts, and the
banner always shows which model is active — handy for comparing the same prompt across models. A
profile can still set `base_url`/`api_key` inline instead of referencing a provider.

### Semantic search

Set an `embedding_model` on a profile (same provider, a different model than the one used for
chat) to enable `/index` and the `search_code` tool:

```toml
[provider]
embedding_model = "text-embedding-3-small"
```

Run `/index` to build the index, then ask a question the model can answer with `search_code`
instead of literal `grep`:

```text
> /index
Indexed 42 file(s) (118 new chunks), skipped 0 unchanged, removed 0 deleted. 118 chunk(s) total.

> Where do we validate the API key?
```

How it works: `/index` walks the project the same `.gitignore`-aware way `grep`/`glob` do, splits
each file into roughly 50-line chunks, preferring blank lines and common declaration starts near the
target so chunks stay more coherent, and embeds any chunk from a file whose content changed since
the last run (tracked by a content hash, so re-running `/index` after a small edit only re-embeds
the files that actually changed). Embeddings are sent in bounded batches, and a file's old index is
replaced transactionally only after every new embedding succeeds. A file the embedding endpoint
refuses is skipped and named in the summary rather than failing the run — aborting made the project
permanently unindexable, since a re-run skips what is already stored and reaches the same bad file
again. If nothing succeeds at all the run does stop, because that is the endpoint, not the files. `search_code` combines SQLite
FTS5 identifier/path candidates with locality-sensitive embedding buckets, then runs exact cosine
ranking over that shortlist. Small indexes still use exhaustive ranking for maximum recall. Results
are returned as `path:start-end` with the chunk text. Without an `embedding_model` configured,
`search_code` is not offered to the model at all rather than erroring.

The index is per project. Kamui keeps one database for everything, but each indexed chunk records
which project root it came from, so indexing several projects is safe and `search_code` only ever
sees the one you launched in.

Once a file is in the index, Kamui keeps it current: after a turn that edited files with
`patch_file`, any of them already in the index is re-embedded before the next prompt.

```text
(refreshed 2 file(s) in the code index)
```

That is a refresh, not a growth — a file the turn *created* is not added on Kamui's initiative.
Stale text in an indexed file is worse than a missing one: `search_code` would confidently quote
code that no longer exists at those lines, while an unindexed file is merely absent from results.
So the correctness problem gets fixed automatically, and widening what's indexed stays your call.

Building the index in the first place never happens on its own — it costs embedding calls against
your own API key, so Kamui will not spend that budget without being asked. What it does instead is
tell you when the index has drifted. If the project has been indexed before, the startup banner
reports what changed:

```text
Index: 3 changed, 1 new since last /index — run /index to refresh.
```

That check compares file modification times against when each file was indexed. It reads no files
and makes no network calls, so it costs nothing at startup — which also makes it a hint rather than
a verdict (a fresh checkout can bump mtimes without changing content). `/index` still compares
content hashes before re-embedding anything, so acting on a false alarm is cheap. Nothing is printed
when the index is up to date, when the project has never been indexed, or in `-p` mode.

The index records its embedding model, so switching models automatically re-embeds unchanged files
instead of comparing incompatible vectors. Chunk boundaries still use lightweight language-neutral
heuristics rather than a full parser, while FTS5 plus LSH avoids loading every vector for larger
projects.

### Cost tracking

Kamui already records how many input and output tokens each request used and which model produced
them. Tell it what your models cost and `/stats` and `/usage` report money as well as tokens:

```toml
[pricing]
currency = "$"                  # optional, display only — Kamui never converts currencies

[pricing.models."gpt-4o"]
input_per_million = 2.50
output_per_million = 10.00

[pricing.models."gpt-4o-mini"]
input_per_million = 0.15
output_per_million = 0.60
```

Prices are **per million tokens**, the same unit every provider quotes on its pricing page, so you
can copy the two numbers across without converting anything. Input and output are separate rates
because providers charge more for output; both are required. The key is the model identifier your
provider expects — the same string as `model` in your profile — matched ignoring case and
surrounding spaces, with no wildcards. Quote it if it contains a `/` or a `.`.

Prices are not secrets and grant nothing, so a project `kamui.toml` may set them too; per-model
entries there override the global ones and can add models the global file never listed.

```text
> /stats

Session:       Fix the streaming parser
Requests:      6
Input tokens:  1500400
Output tokens: 49020
Total tokens:  1549420
Cost:          $3.4012+

--- Per model ---
  gpt-4o                     4 req   1200000 in     40000 out   1240000 total       $3.4000
  codeqwen:latest            2 req    300000 in      9000 out    309000 total      unpriced

Some usage came from a model with no price in [pricing.models]; it is excluded, not free.
```

Two rules make the numbers trustworthy. A model you have not priced is reported as `unpriced`,
never as `$0.0000` — Kamui will not tell you something was free when it does not know. And a total
that mixes priced with unpriced usage carries a trailing `+`, so an amount never quietly stands for
less spend than actually happened. Cost covers every request that was billed, including the small
call that generates a session title.

All of this is optional and off by default. With no `[pricing]` section, `/stats` and `/usage` look
exactly as they did before — no cost column, no empty cells, no zeroes.

### Skipping approval

Every `run_command`/`patch_file` call asks for approval by default, even for something as safe as
`git status`. Two ways to reduce that friction, in the global `kamui.toml` only — never in a
project file, since either one would let a checked-in file grant unattended execution:

```toml
[permissions]
allow_commands = ["git status", "git diff", "cargo check"]
```

`allow_commands` is an exact match (after trimming) against the whole command string, so
`"git status"` does not also cover `"git status --short"` — list each command you want to skip.

For everything else, start Kamui (or `-p`) with `--auto-approve` to skip every confirmation prompt
for the whole session:

```sh
kamui --auto-approve
kamui --auto-approve -r <session-id>
```

Kamui prints a banner when it starts this way, since it removes every safety prompt for the
session. `-p`'s `--auto-approve` (see [Non-interactive mode](#non-interactive-mode)) works the
same way for a single scripted turn.

## Install

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/algonacci/kamui/main/install.ps1 | iex
```

Linux and macOS:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/algonacci/kamui/main/install.sh | sh
```

Then open a new terminal and run:

```sh
kamui
```

Check the installed version with `kamui --version` and list command-line options with
`kamui --help`.

For development, install the current checkout into Cargo's binary directory:

```sh
cargo install --path .
```

This compiles once and installs `kamui` in `~/.cargo/bin`. It does not compile again each time the
command runs. Use `cargo install --path . --force` after local source changes.

## Development

```sh
cargo run
```

Kamui stores sessions, messages, and token usage in a local SQLite database. Each launch starts a
new chat, but it is only saved as a session after the first successful response. Use `/sessions`
and `/resume <id>` to continue an earlier conversation. Use `/help` inside the chat to list all
session commands.

The startup status card shows the Kamui version, project path, Git branch and changed-file count,
active model, available tools, connected MCP servers, and project instruction file. Run `/status`
at any time to refresh it after repository changes.

After the first response, the active provider generates a short session title. `/sessions` shows
that title together with the last-updated timestamp, message count, and total token usage. Tokens
used to generate the title are included in session usage analytics.

Resume a saved session directly when starting Kamui:

```sh
kamui -r <session-id>
```

### Non-interactive mode

Run a single prompt and exit, for scripting or CI:

```sh
kamui -p "explain what this project does"
```

The prompt runs through the same agent loop as interactive chat — including tools — and the
exchange is saved as a resumable session, just like a normal chat turn. Tool calls that need
approval (`run_command`, `patch_file`) are denied by default in this mode, since there is no one to
ask; pass `--auto-approve` to run them without prompting:

```sh
kamui -p "run the test suite and summarize failures" --auto-approve
```

### Diagnostics

Check that everything is configured and reachable without starting a chat session:

```sh
kamui doctor
```

It parses the config, tests chat and embedding endpoints, connects to every configured MCP server,
checks the project root, runs SQLite's integrity check, and reports the optional RTK binary. It
prints a pass/fail line for each check and exits non-zero if a required check failed.

For a quicker look with no network calls at all:

```sh
kamui status
```

`status` reads the config file and local database directly and prints a summary — active profile
and model, configured profiles, `embedding_model`, MCP servers, the command allowlist, project
path, database path, session count, remembered-fact count, and this project's indexed-chunk count.
Unlike
`doctor`, it never contacts the provider or an MCP server, so it works even when those would fail.

### Benchmark mode

Create a JSON suite and run it repeatedly against the default or a named profile:

```json
{
  "cases": [
    {
      "name": "rust-basics",
      "prompt": "Name Rust's ownership rules in one paragraph.",
      "expect_contains": ["ownership", "borrow"]
    }
  ]
}
```

```sh
kamui benchmark suite.json --profile sol --runs 3
```

Each run reports pass/fail, latency, and tokens; the command exits non-zero when an expectation is
missing, making the same suite usable locally and in CI. Expectations are optional and matched
case-insensitively.

### Scheduled jobs

Kamui has a persistent SQLite-backed command queue. Jobs remain scheduled across restarts and run
when a local worker is active:

```sh
kamui jobs add --now -- cargo test
kamui jobs add --at 2026-08-05T09:00:00+07:00 -- cargo test --release
kamui jobs add --every 6h -- git fetch --all
kamui jobs list
kamui jobs worker
```

Use `kamui jobs worker --once` to drain commands that are due and exit, which is convenient from an
OS scheduler. `cancel`, `pause`, and `resume` accept a job id. Jobs execute in the directory where
they were created, have a 30-minute safety timeout, retain capped stdout/stderr and exit status,
and missed recurring runs are coalesced rather than replayed in a burst. This queue is separate from
the temporary `run_command(background: true)` jobs owned by an active chat process.

### Interactive transcript UI

When launched from an interactive terminal, Kamui uses a fullscreen, opencode-style interface: a
word-wrapped transcript with thick-border message rails (user = blue, assistant = brand blue with
Markdown styling, tool calls muted, errors red), a sidebar with session info and live context
usage, an always-live editor box with slash-command menu, and a quiet footer (`?` opens the keymap).

Highlights:

- **Home screen** — a two-tone block-letter KAMUI logo greets you until the first message.
- **Everything is a cell** — user messages, tool calls, answers, and command output all render as
  transcript cells in the order they happened. A slash command opens a cell headed by the command
  you typed and its output lands inside it, so `/sessions` output is attributed and stays put
  instead of sinking into a flat run of status lines below every card.
- **Steering** — keep typing while the agent works. Your message is folded into the *running*
  turn at the next tool-round boundary, so a correction reaches the model while it can still act
  on it instead of waiting for the whole turn to finish. If the turn ends before the next
  boundary the message simply starts the next turn. A steered message is labelled as one in the
  transcript — on re-reading a session there is otherwise no way to tell which of two prompts
  started the work. **Esc interrupts** outright when you would rather discard the turn than
  redirect it.
- **Loading state** — the editor stays live and keeps its caret; a bouncing wall inside the
  editor box reports what the run is doing (`Esc interrupts`). The transcript does not
  duplicate that spinner.
- **Scrolling** — mouse wheel, PgUp/PgDn (full page), Ctrl+U/Ctrl+D (half page),
  `Ctrl+Home`/`Ctrl+End`. Typing snaps back to the live tail, and while you are scrolled away from
  it the footer says how far back you are and how to return.
- **Dialogs** — `Ctrl+K` model picker, `Ctrl+S` session switcher, `?` keybinding sheet. Bare
  `/model`, `/sessions`, `/skills`, and `/help` open the same overlays; picking a skill toggles it
  on or off (`/skills toggle <name>` does the same from the prompt). Enter submits through the normal
  command path, so queueing still applies while busy.
- **Transcript search** — `Ctrl+F` searches what is on screen right now (`/search` looks through
  saved sessions in SQLite, which is a different question). Every matching row is washed, the one
  in view brighter, with an `n/m` readout; `Enter` and `Up` step through them wrapping at both
  ends, and `Esc` closes and returns to the live tail.
- **Copying out** — `Ctrl+Y` copies the latest answer as the raw Markdown that was streamed;
  **right-click** a cell to copy that cell (its header and outcome included, so you can tell what
  the text is). Both report what was taken. Mouse capture means drag-select belongs to Kamui, but
  most terminals still fall back to their own selection while **Shift** is held.
- **Background jobs announce themselves** — a job started with `run_command(background: true)`
  reports on the way back to the prompt when it ends, once, with its exit status and how long it
  took. Previously a `cargo test` could have finished minutes ago and only `/jobs` would say so.
- **Modes** — `Tab` cycles **build → auto → plan**, `Shift+Tab` goes back, and `/mode
  [build|auto|plan]` sets one directly. *build* asks before every command and edit; *auto*
  bypasses those approvals for the rest of the session (what `--auto-approve` starts in); *plan*
  restricts the agent to read-only tools until you approve a plan. The active mode is on the
  sidebar. Tab still accepts a slash completion while the command menu is open, so the two never
  compete for the key.
- **Permission modal** — tool and plan approvals render as "Allow once / Always allow this
  session / Reject" with the diff or command preview inline. The preview fills the room the
  terminal has and pages with `PgUp`/`PgDn`, always saying which lines of how many you are
  looking at — a diff longer than the box must never be cut without saying so, since the point
  of the modal is that you can read what you are agreeing to.
- **Model registry** — the model dialog ends with "+ Add provider / model": enter a base URL and
  API key, Kamui fetches the provider's live model list and registers your pick as a new profile
  in the global config, then switches to it.
- **Real editing** — the caret moves. `←`/`→` by character, `Alt`+`←`/`→` by word,
  `Home`/`End` or `Ctrl+A`/`Ctrl+E` to the ends of the line, `Delete` forward, `Ctrl+W` to rub out
  the word behind the caret. Text is inserted where the caret is, and a line longer than the box
  scrolls horizontally to keep the caret in view. (Jumping the transcript to top/bottom moved to
  `Ctrl+Home`/`Ctrl+End`.)
- **Multiline input** — `Shift+Enter`, `Ctrl+Enter`, `Alt+Enter`, or `Ctrl+J` starts a new line
  without sending; terminals disagree about which of those they report, so all are accepted.
  Ending a line with `\` and pressing Enter still works. The editor box grows as you type.
- **Pasting** — a multi-line paste arrives as one message, newlines and all, instead of each line
  submitting itself. Images cannot travel through a terminal paste at all; use `@clipboard` to
  read a screenshot straight from the system clipboard, or `@shot.png` for a file. Either way the
  transcript reports what was attached (`4 file(s) attached · 1 image(s) (image/png ~210 KiB)`),
  since the prompt itself only ever shows the reference you typed. Files a directory reference had
  to leave out are reported too — an `@src` that delivered twelve of fifty files answers from
  partial context, and until now only the model was told.
  `Ctrl+Shift+V` inserts `@clipboard` at the caret without submitting. The fullscreen footer shows
  compact indicators for valid file, directory, image, diff, staged, and clipboard references;
  incomplete or invalid references are hidden.
- **Tool cards** — a call and its result are one block: a header naming the tool and its
  arguments, then an outcome row (`✓ completed · 1.2s · 142 chars`, or `✗ failed · 1.2s`) that
  stays visible while the output itself stays folded. **`Ctrl+O` or a click** on the card
  expands or folds it; `/expand` // `/collapse` do the same from the prompt. All three act on
  the newest cell that actually has rows folded away, so a command's own output cell never
  steals the target. Multi-line command
  output renders as proper lines instead of one run-on blob.
- **Sidebar** — `Ctrl+B` hides or shows it, and it narrows from 30 to 24 columns before giving up
  entirely on a terminal under 68 columns wide. Carries session, id, version, model (marked `· no tools` when the active profile sets
  `tools = false`, since a model offered no tools invents tool-call syntax in prose and looks
  broken rather than switched off), project, MCP servers with their live tool counts
  (a server that failed to start reads `unavailable` rather than quietly disappearing), context
  usage, and last-turn metrics.
- **Unknown commands name themselves** and suggest the nearest built-in (`/sesions` → "Did you
  mean /sessions?").

`-p`, pipes, and redirects retain the script-friendly line-oriented output path. On an interactive
TTY, `NO_COLOR` keeps the fullscreen TUI but removes its semantic foreground/background colours.

### Session commands

| Command | Description |
| --- | --- |
| `/new` | Start a new session |
| `/sessions` | List saved sessions |
| `/resume <id>` | Resume a session |
| `/model [name]` | List provider profiles, or switch to one |
| `/rename <id> <title>` | Rename a session |
| `/search <text>` | Search saved messages across all sessions |
| `/compact` | Summarize older messages to free up context |
| `/undo` | Revert the files patched by the last turn |
| `/jobs` | List temporary session jobs and persistent scheduled jobs |
| `/index` | Rebuild the semantic-search index (needs `embedding_model`) |
| `/commands` | List your own prompt commands |
| `/delete <id>` | Delete a session |
| `/stats` | Show current session usage |
| `/usage` | Show token usage by day and month, across all sessions |
| `/status` | Show project and connection status |
| `/memory` | List facts Kamui remembers across sessions and projects |
| `/forget <text>` | Forget one remembered fact, or `/forget all` |
| `/expand` | Expand the latest transcript card |
| `/collapse` | Collapse the latest transcript card |
| `/mode [build\|auto\|plan\|next\|prev]` | Show or change what a turn may do on its own; `Tab` / `Shift+Tab` cycle it |
| `/warnings [on\|off\|details\|fix]` | Hide/show, expand details of, or hand skill warnings to Kamui as a repair task. After a `fix` turn Kamui reloads the skill loader and reports what actually changed — how many folders now load, how many still fail, and whether the repair broke a folder that used to work — rather than leaving the old warning banner in place |
| `/help` | List available commands |
| `/exit` | Save and quit |

Malformed or unreadable user/project `settings.json` files are reported in the startup warning rail
and by `/warnings details`; Kamui does not silently overwrite them. Custom theme JSON files are also
validated before selection, and every resolved color must use exact `#RRGGBB` form. Invalid themes
are omitted from the picker and the last valid/default theme remains active.

The model picker shows the profile, model, an active marker, and short capability notes (`tools off`,
`Coding/sticky`, context window). Empty model or session pickers say so instead of opening a blank
dialog. While a turn is running, `/model` and `/resume` are rejected rather than queued; typed input
is queued for the next agent step and only folded into the current turn between tool rounds.

If the provider streams reasoning (`reasoning`, `reasoning_content`, or `thinking`), Kamui shows a
collapsed Thinking card. Click the card, the bouncing wall, or the footer hint — or press `Ctrl+T` —
to expand or hide it. Reasoning is display-only and is not sent back to the model.

`Ctrl+C` or `Esc` while a turn is running — waiting on the model, streaming, at an approval
prompt, or running a command — cancels that turn and returns you to the prompt, killing any
running command. The cancelled turn is not saved. At the idle prompt, `Ctrl+C` must be pressed
twice within three seconds to exit (a hint appears on the first press).

`/rename` accepts a session ID prefix followed by the new title; if the renamed session is the
active one, its in-memory title updates immediately. `/search` matches message text
case-insensitively (literal `%` and `_` are not treated as wildcards) and prints each hit as its
session ID, timestamp, title, and a snippet centered on the match.

After each streamed response, Kamui reports time-to-first-token (`TTFT`) and total response time
(`Time`) alongside token usage and the finish reason. The line-oriented terminal feed shows compact
tool arguments followed by completion/failure state, elapsed time, and output size. Responses use
light markdown styling. While the model thinks, a braille spinner runs inline on the scrollback;
in fullscreen TUI a bouncing wall in the editor is the in-flight indicator. Styling and the thinking
spinner are disabled when input/output is not a
terminal; `NO_COLOR` disables colour while retaining the structured feed.

`/stats` covers the current session; `/usage` zooms out to every session, reporting tokens per day
and per month so you can see what a week of work actually cost. Configure
[cost tracking](#cost-tracking) and both report money alongside the tokens.

Long conversations are compacted automatically: once the recent history grows large, Kamui folds the
older messages into a running summary so the session can continue without overflowing the model's
context. Run `/compact` to do it on demand. The full history is always kept in storage — only what
is sent to the model each turn is compressed.

### Your own commands

Any markdown file can become a slash command. Drop it in one of two places:

```text
<project>/.kamui/commands/review.md    ->  /review   (this project only)
<config dir>/kamui/commands/review.md  ->  /review   (every project)
```

The config directory is the same one holding your global `kamui.toml` (see
[Configuration](#configuration)). A project command shadows a global one with the same name, so a
repo can specialise `/review` without losing your general-purpose one everywhere else.

Invoking `/review` sends that file's contents as your prompt. Anything typed after the command is
appended below it, so file references still work:

```text
> /review @src/auth.rs
```

This is plain text expansion — exactly what you would get by pasting the file in yourself, which
means a command grants no new permission and approval prompts behave as usual. Commands work in
`-p` too (`kamui -p "/review"`), so they are scriptable.

An optional frontmatter block sets the description shown by `/commands`:

```markdown
---
description: Pragmatic code reviewer — sharp, practical, no fluff
---

You are a Senior Code Reviewer...
```

Frontmatter is optional, and any keys Kamui does not recognise are ignored — a prompt file written
for another tool (with `tags:`, `agents:`, and so on) can be dropped in unchanged. Files named
after a built-in command are ignored, so nothing can shadow `/new` or `/exit`.

Commands are independent: there is no chaining or pipeline between them. Each one can be invoked
at any time, in any order, as often as you like.

## Repository context

Kamui uses the directory where it was launched as the project root. If that directory contains
`KAMUI.md` or `AGENTS.md`, Kamui sends the first file found in that order as project instructions
with every chat request.

Reference a UTF-8 text file relative to the project root with `@path`:

```text
> Explain the error handling in @src/main.rs
> Summarize @"docs/My Notes.md"
```

Reference a directory to attach everything inside it:

```text
> Review the error handling across @src
```

Directory attachment honours `.gitignore` and skips hidden files, so build output and dependencies
stay out. Files are attached until the context budget or a 50-file cap runs out; the rest are noted
as omitted instead of failing the prompt.

Referenced files are attached only to that request and are not copied into session history. Each
file is limited to 64 KiB and all attached files together are limited to 128 KiB. Absolute paths,
binary files, and paths or symlinks outside the project are rejected. Quote references that contain
spaces with `@"path with spaces.md"` or `@'path with spaces.md'`.

Use `@diff` for unstaged tracked changes or `@staged` for changes in the Git index:

```text
> Review @diff for bugs
> Write a commit summary for @staged
```

Git context is read-only and can be combined with file references. Untracked files are not included
in `@diff`; attach them explicitly with `@path`.

Use `@clipboard` to attach whatever is on your system clipboard — text or an image:

```text
> Why does this happen? @clipboard
```

If the clipboard holds text (an error message, a stack trace, a snippet) it is attached as text. If
it holds an image, it is attached as an image — so you can take a screenshot (`Win+Shift+S` on
Windows, `Cmd+Shift+Ctrl+4` on macOS) and paste it straight into a prompt without saving a file
first. Terminals cannot receive pasted image data directly, so `@clipboard` is how images get in.

You can also reference an image file. `.png`, `.jpg`, `.jpeg`, `.gif`, and `.webp` are attached as
images rather than inlined as text:

```text
> What is wrong with this layout? @screenshot.png
```

Each image is limited to 5 MiB and is sent only with that request. The active model must support
image input; text-only models will reject it.

## Tools

Kamui offers the model these tools: `list_directory` (discover what is in a folder), `read_file`
(read UTF-8 text), `read_image` (decode and attach PNG, JPEG, WebP, or GIF pixels for a
vision-capable model), `grep` (search file contents by regular expression), `glob` (find files by a
glob pattern), `run_command` (run a shell command, optionally in the background), `command_status`/
`stop_command` (check on or stop a background job), `patch_file` (edit or create a file),
`update_plan` (declare a live checklist for a multi-step task), `spawn_agent` (delegate a
self-contained, read-only task to an isolated sub-agent), `search_code` (semantic code search,
only offered when `embedding_model` is configured — see below), `ask_user` (pause and ask you a
clarifying question), and `remember`/`update_memory`/`forget` (manage persistent memory — see
below). When you ask about code, the model can explore, read, build, test, and fix on its own
instead of requiring you to attach files with `@path`:

```text
> What does the agent loop in src/chat.rs do?
> Run the tests and tell me if anything fails.
> Fix the typo in the README heading.
```

`grep` and `glob` are read-only, so they never prompt for approval. Both respect `.gitignore` the
same way directory context (`@dir`) does. `grep` takes a regular expression plus optional `path`
(scope), `glob` (filename filter), and `case_insensitive`; `glob` takes a pattern like
`"src/**/*.rs"` plus an optional `path` scope. The model is steered to prefer these over shelling
out to `grep`/`find` through `run_command`, since they need no approval and are cheaper to run.

For multi-step tasks, the model can call `update_plan` with a checklist (`{step, status}` for each
step, replacing the whole list each call) instead of leaving its progress buried in the raw tool
trace. Kamui renders it live as a checklist:

```
  → plan
    [x] Explore the codebase
    [~] Implement the change
    [ ] Run tests
```

`[x]` is completed, `[~]` is in progress, `[ ]` is pending. Like `grep`/`glob`, it never prompts
for approval — it has no effect outside the conversation and round-trips through session history
like any other tool call, so a resumed session shows the plan as it stood at each point.

If the model calls a tool, Kamui prints a short trace of each call, runs it, feeds the result back,
and continues streaming until a final answer. The read tools reuse the same path safety as `@file`
(project-relative only, no escaping the root, 64 KiB per file) and the loop is bounded so it cannot
run away.

`run_command` never runs on its own. Kamui shows you the exact command and waits for you to approve
it (`y`/`yes`); anything else declines and tells the model so. A third answer, `a`/`always`, both
approves this call and grants that *tool* (not that specific command — every future call to it,
e.g. any `run_command` invocation) a standing pass for the rest of the active session, so further
calls skip the prompt entirely until `/new` clears it. This is separate from the global
`[permissions] allow_commands` allowlist below: "always allow" is a one-tap, session-only grant you
make from the prompt itself, not something you configure ahead of time. Commands run in the project
directory with input disabled and a capped amount of captured output, and the model sees the exit
code alongside stdout and stderr. The foreground timeout defaults to 30 seconds and is configurable —
see `[commands]` below.

For something that legitimately runs longer than that — a dev server, a slow test suite — the model
can pass `background: true` instead. It returns a job id immediately rather than waiting, and can
check on it with `command_status` (omit `job_id` to list every job) or stop it early with
`stop_command`. `/jobs` lists them directly without going through the model. Background jobs are
in-memory only: they are killed when Kamui exits (including a plain `-p` run) and do not survive a
restart, and each has a `background_max_secs` (default 30 minutes) safety cap against a runaway or
zombie process — not a limit meant to constrain a legitimately long-running command.

```toml
[commands]
timeout_secs = 30            # foreground run_command
background_max_secs = 1800   # safety cap for a background: true job
```

`patch_file` edits one file per call and is also gated behind your approval (the same `y`/`yes`/
`a`/`always` prompt): Kamui shows the change as removed (`-`) and added (`+`) lines before asking. A
patch replaces text that must match the file
exactly once — if it does not, the patch is rejected and the model is told to re-read the file, so a
stale edit can never overwrite unexpected content. An empty `old_text` creates a new file. Writes are
atomic per file, and paths cannot escape the project root.

Each file `patch_file` touches is approved individually, exactly as before, but Kamui also keeps a
snapshot of what every touched file looked like before the turn started. If a multi-file edit is
interrupted with `Ctrl+C` partway through, the files it already changed are automatically reverted
so the turn never leaves the repository half-edited with no trace in session history. `/undo`
reverts the same way for a turn that *did* complete — one level, most recent turn only; a second
`/undo` has nothing left to do.

If the [RTK](https://github.com/rtk-ai/rtk) binary is installed, simple approved commands are
automatically prefixed with `rtk` so their output is compressed before it reaches the model. RTK is
optional: commands with shell operators, commands whose first word the shell runs itself (`cd`,
`set`, and other builtins, or a `VAR=value` prefix), and systems without RTK always run directly,
and the first line of every result shows the exact command that ran.

The whole turn is saved to session history, including the tool calls and their results, so a resumed
session replays the tool interactions the model relied on.

`ask_user` pauses a turn so the model can ask you something instead of guessing — which of several
matching files was meant, a preference between reasonable options. Kamui prints the question (with
numbered choices, if any) and waits for one line of input; replying with a number resolves to that
option's text, and anything else is taken as typed. This is not an approval prompt — `run_command`
and `patch_file` still ask for that automatically — it's for when the model itself decides it needs
more information to continue. In `-p` non-interactive mode there is no one to ask, so the tool is
declined and the model is told to proceed on its own judgment instead.

## Sub-agents

`spawn_agent` delegates a self-contained task to an isolated sub-agent and returns only its final
answer — the sub-agent's own tool calls and intermediate output never enter the main
conversation's context:

```text
> Summarize how error handling works across the codebase
```

The model might call `spawn_agent` internally to explore before answering, instead of doing that
exploration in the main conversation itself. The sub-agent:

- Starts fresh with no memory of the conversation, so its prompt must be self-contained.
- Can only `read_file`, `list_directory`, `grep`, and `glob` — it cannot run commands, edit files,
  touch memory, or spawn another sub-agent, so it never needs your approval for anything it does.
- Runs concurrently when the model issues several independent `spawn_agent` calls in one response.
  Kamui caps each batch at four and returns results in the original tool-call order. The parent turn
  still waits for the batch; read-only isolation means no approval prompts can interleave.
- Uses a derived sticky session/cache ID per `spawn_agent` call, so all rounds of that sub-agent stay
  together without disturbing the main conversation's cached prompt.

This is a good fit for a well-scoped question like "find every place X is used and summarize how"
or "explain what module Y does," not a substitute for tools the model can already call directly.

## Memory

Kamui can remember facts across every session and project, not just the current conversation. Ask
it directly, or state a clear preference:

```
> Remember that I prefer bun over node, and uv over pip.
```

The model calls `remember` to store the fact permanently. Unlike project instructions (`KAMUI.md`,
a file you edit yourself) or session history (scoped to one conversation), memory is global: it
survives `/new`, resuming a different session, and switching to a different project entirely, and
is read fresh on every turn — no restart needed to see a fact you just asked it to remember.

- `/memory` lists everything currently remembered.
- `/forget <text>` removes one fact matched by an unambiguous substring of its exact wording, or
  `/forget all` clears everything.

The model can also correct or remove a fact itself: `update_memory` replaces an existing entry
matched the same way `/forget` does, so a stated preference can be corrected in place instead of
contradicting an older one; `forget` removes one. Both fail with a clear error if the text matches
more than one stored fact, asking for something more specific rather than guessing. Total stored
memory is capped at 4 KiB, since every byte of it is sent with every request.

## MCP servers

Kamui is an [MCP](https://modelcontextprotocol.io) client. Declare servers in your **global**
`kamui.toml` and Kamui launches them at startup, offering their tools to the model alongside the
built-in ones:

```toml
[mcp.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]

[mcp.excel]
command = "uvx"
args = ["mcp-excel"]
trusted = true          # skip the per-call approval for this server
```

Their tools appear as `<server>__<tool>` (for example `excel__read_sheet`) so they cannot collide
with the built-ins. Every MCP call asks for your approval first — third-party servers can do
anything — unless you mark that server `trusted`. A server that fails to start is reported and
skipped, so a broken entry never prevents Kamui from running.

Servers may only be declared in the global config: launching one is arbitrary code execution, so a
checked-in project file is not allowed to do it. Only stdio servers are supported today.

## RTK integration direction

[RTK](https://github.com/rtk-ai/rtk) is an optional execution backend for the `run_command` tool.
Supported commands run through RTK so compact test, build, search, Git, and container output reaches
the model. Kamui still owns command permissions, timeouts, cancellation, output limits, and the
recorded command line; RTK only compresses output.

RTK is not currently used by chat or `@diff`, and users do not need to install it yet. Keeping raw
diff context avoids dropping details needed for code review. The future integration will detect the
external `rtk` binary and fall back to direct execution when it is unavailable or a command is not
supported.

## Data storage

The database uses the standard local application data directory for Windows, macOS, and Linux.
Set `KAMUI_DATA_DIR` to override it, particularly for servers and containers:

```env
KAMUI_DATA_DIR=/var/lib/kamui
```

SQLite is bundled into the Kamui binary, so users do not need to install it separately. For a
container deployment, mount `KAMUI_DATA_DIR` as a persistent volume. Each device has its own local
database; future multi-device synchronization should exchange records through an API rather than
copying the database file.
