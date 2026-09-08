use crate::context::{
    list_project_directory, read_project_file, resolve_for_write, resolve_within_root,
};
use crate::provider::{ImageAttachment, ToolCall, ToolDefinition};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Hard limit on captured command output: a chatty command cannot flood the model's context. The
/// actual timeout is `CommandLimits`, configurable via `kamui.toml`'s `[commands]` section.
const MAX_COMMAND_OUTPUT: usize = 16 * 1024;

/// Hard limits for `grep`/`glob`: how much matched text/how many matches are returned, and the
/// largest file `grep` will read into memory to scan (generous, since it never attaches full
/// content the way `read_file` does, only matching lines).
const MAX_SEARCH_OUTPUT: usize = 16 * 1024;
const MAX_SEARCH_MATCHES: usize = 200;
const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;

/// A capability the model can invoke by name. Read-only tools run without prompting; anything with
/// side effects returns `true` from `requires_confirmation` so the chat loop asks the user first.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    fn requires_confirmation(&self) -> bool {
        false
    }
    /// Like `requires_confirmation`, but given the call's arguments — lets a tool exempt specific
    /// calls (e.g. an allowlisted `run_command` command) while still confirming everything else.
    /// Defaults to `requires_confirmation`, so most tools never need to override this.
    fn requires_confirmation_for(&self, _arguments: &str) -> bool {
        self.requires_confirmation()
    }
    /// A human-readable preview of what this call would do, shown before asking for confirmation.
    fn preview(&self, _arguments: &str) -> Option<String> {
        None
    }
    async fn run(&self, arguments: &str) -> Result<String>;
}

/// The set of tools offered to the model, and the dispatcher that runs a requested call.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    project_root: PathBuf,
    jobs: JobRegistry,
}

impl ToolRegistry {
    /// The built-in project tools, plus any externally provided ones (e.g. from MCP servers).
    /// `allow_commands` is `run_command`'s exact-match allowlist of commands that skip confirmation
    /// (global config only — see `config::Config::allow_commands`).
    pub fn with_defaults(
        project_root: PathBuf,
        extra: Vec<Box<dyn Tool>>,
        allow_commands: Vec<String>,
        command_limits: CommandLimits,
    ) -> Self {
        let jobs: JobRegistry = Arc::new(Mutex::new(HashMap::new()));
        let mut tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ReadFileTool {
                root: project_root.clone(),
            }),
            Box::new(ReadImageTool),
            Box::new(ListDirectoryTool {
                root: project_root.clone(),
            }),
            Box::new(GrepTool {
                root: project_root.clone(),
            }),
            Box::new(GlobTool {
                root: project_root.clone(),
            }),
            Box::new(UpdatePlanTool),
            Box::new(RunCommandTool {
                root: project_root.clone(),
                allow_commands,
                timeout: command_limits.timeout,
                background_max: command_limits.background_max,
                jobs: jobs.clone(),
            }),
            Box::new(CommandStatusTool { jobs: jobs.clone() }),
            Box::new(StopCommandTool { jobs: jobs.clone() }),
            Box::new(PatchFileTool {
                root: project_root.clone(),
            }),
        ];
        tools.extend(extra);
        Self {
            tools,
            jobs,
            project_root,
        }
    }

    /// A read-only registry for an isolated sub-agent (see `spawn_agent`): only
    /// `read_file`/`list_directory`/`grep`/`glob`, none of which ever require confirmation, so a
    /// sub-agent has no approval flow to reproduce and cannot mutate anything, run commands, or
    /// spawn another sub-agent.
    pub fn read_only(project_root: PathBuf) -> Self {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ReadFileTool {
                root: project_root.clone(),
            }),
            Box::new(ReadImageTool),
            Box::new(ListDirectoryTool {
                root: project_root.clone(),
            }),
            Box::new(GrepTool {
                root: project_root.clone(),
            }),
            Box::new(GlobTool {
                root: project_root.clone(),
            }),
        ];
        Self {
            tools,
            jobs: Arc::new(Mutex::new(HashMap::new())),
            project_root,
        }
    }

    /// The shared background-job registry, so callers that aren't going through the tool-call
    /// protocol (the chat loop's `/jobs` command, killing jobs on shutdown) can reach it directly.
    pub fn jobs(&self) -> JobRegistry {
        self.jobs.clone()
    }

    /// Read an image for the chat loop, which must carry it as multimodal content.
    pub fn read_image(&self, arguments: &str) -> Result<(String, ImageAttachment)> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let path = value
            .get("path")
            .and_then(|path| path.as_str())
            .context("read_image requires a 'path' string argument")?;
        if let Some(detail) = value.get("detail") {
            let detail = detail
                .as_str()
                .context("read_image detail must be a string")?;
            if !matches!(detail, "low" | "high" | "auto") {
                anyhow::bail!("read_image detail must be low, high, or auto");
            }
        }
        let image = crate::context::read_project_image(&self.project_root, path)?;
        Ok((image.metadata(), image.attachment))
    }

    /// A preview of what a confirmation-gated call would do, if the tool provides one.
    pub fn preview(&self, call: &ToolCall) -> Option<String> {
        self.tools
            .iter()
            .find(|tool| tool.name() == call.name)
            .and_then(|tool| tool.preview(&call.arguments))
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<ToolDefinition> =
            self.tools.iter().map(|tool| tool.definition()).collect();
        definitions.push(ask_user_definition());
        definitions.push(spawn_agent_definition());
        definitions.extend(memory_definitions());
        definitions
    }

    /// Just the built-in/extra tool definitions, without the ask_user/spawn_agent/memory
    /// pseudo-tools — for a context (spawn_agent's own isolated sub-loop) that has no interactive
    /// stdin, `Database`, or `Provider` access to service those, and must not be able to recurse
    /// into another sub-agent.
    pub fn tool_definitions_only(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether a named call must be confirmed by the user before it runs — the arguments let a
    /// tool exempt a specific call (e.g. an allowlisted `run_command` command) while still
    /// confirming everything else. Unknown names are treated as not requiring confirmation;
    /// dispatch will report them as an error anyway.
    pub fn requires_confirmation_for(&self, name: &str, arguments: &str) -> bool {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(|tool| tool.requires_confirmation_for(arguments))
            .unwrap_or(false)
    }

    /// Run a requested call. Failures are returned as an `Error: ...` string rather than propagated
    /// so the model can read the problem and recover on the next turn.
    pub async fn dispatch(&self, call: &ToolCall) -> String {
        if call.name == "read_image" {
            return match self.read_image(&call.arguments) {
                Ok((metadata, _)) => metadata,
                Err(error) => format!("Error: {error:#}"),
            };
        }
        match self.tools.iter().find(|tool| tool.name() == call.name) {
            Some(tool) => match tool.run(&call.arguments).await {
                Ok(output) => output,
                Err(error) => format!("Error: {error:#}"),
            },
            None => format!("Error: unknown tool '{}'", call.name),
        }
    }
}

/// Name of the pseudo-tool the chat loop intercepts before it reaches `ToolRegistry::dispatch`.
/// It has no `Tool` implementation of its own: it needs access to the interactive stdin channel
/// (`start_chat`'s `input_rx`), which `Tool::run` has no way to receive, so the chat loop handles
/// it directly instead. Its definition still comes from `ToolRegistry::definitions()` so it is
/// offered to the model like any other tool and stays in one place with the rest of the roster.
pub const ASK_USER_TOOL: &str = "ask_user";

fn ask_user_definition() -> ToolDefinition {
    ToolDefinition {
        name: ASK_USER_TOOL.to_string(),
        description: "Pause and ask the user a clarifying question before proceeding, when a \
                      task is ambiguous or has multiple reasonable interpretations. Call this \
                      tool itself — do not just write the question as your response text, since \
                      that does not pause the turn or wait for an actual answer, only continue \
                      the conversation on the next message. This is not for approval of a risky \
                      action (run_command and patch_file already ask for that automatically) — \
                      use it when you genuinely need more information to continue, e.g. which of \
                      several matching files was meant, or a preference between reasonable \
                      options."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask, e.g. \"Which file did you mean?\""
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "2-4 short suggested answers shown as numbered choices, e.g. [\"src/main.rs\", \"src/lib.rs\"]. Optional — omit for an open-ended question with no natural fixed choices."
                }
            },
            "required": ["question"]
        }),
    }
}

/// Name of the pseudo-tool that runs an isolated sub-agent for a self-contained delegated task
/// (e.g. exploring part of the repository) and returns just its final answer. Intercepted by the
/// chat loop the same way `ask_user` is: it needs the active `Provider`/model and
/// `ProjectContext`, which `Tool::run` has no way to receive.
pub const SPAWN_AGENT_TOOL: &str = "spawn_agent";

fn spawn_agent_definition() -> ToolDefinition {
    ToolDefinition {
        name: SPAWN_AGENT_TOOL.to_string(),
        description: "Delegate a self-contained task to an isolated sub-agent and get back just \
                      its final answer, instead of doing the exploration yourself and filling \
                      your own context with the raw trace. The sub-agent starts fresh with no \
                      memory of this conversation, so the prompt must be self-contained. It can \
                      only read the repository (read_file, list_directory, grep, glob) — it \
                      cannot run commands, edit files, or spawn another sub-agent. Good for a \
                      well-scoped question like \"find every place X is used and summarize how\" \
                      or \"explain what module Y does\". For independent tasks, issue up to four \
                      spawn_agent calls in the same response and Kamui runs them concurrently; \
                      keep dependent tasks serial. This is not a substitute for tools you can call \
                      directly."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "A self-contained task description. Include everything the sub-agent needs to know — it has no access to this conversation's history."
                }
            },
            "required": ["prompt"]
        }),
    }
}

/// Names of the memory pseudo-tools, intercepted by the chat loop the same way `ask_user` is:
/// they need access to `Database`, which `Tool::run` has no way to receive. Memory is global and
/// permanent (see `storage::Database::remember`) — unlike project instructions (`KAMUI.md`), which
/// are a file the user edits, these are facts the model itself records when explicitly asked, and
/// unlike session history, they are visible from every project Kamui is run in afterward.
pub const REMEMBER_TOOL: &str = "remember";
pub const UPDATE_MEMORY_TOOL: &str = "update_memory";
pub const FORGET_TOOL: &str = "forget";

fn memory_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: REMEMBER_TOOL.to_string(),
            description: "Save a fact about the user or their preferences permanently. Unlike \
                          project instructions, this is not scoped to the current project — it \
                          persists across sessions and is visible in every project Kamui is run \
                          in afterward. Use it only when explicitly asked to remember something, \
                          or when the user states a clear, durable preference (e.g. a tool or \
                          style they always want used). If a similar fact is already remembered, \
                          use update_memory instead of adding a duplicate."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "fact": {
                        "type": "string",
                        "description": "A short, self-contained fact, e.g. \"Prefers bun over node.\" or \"Prefers uv over pip.\""
                    }
                },
                "required": ["fact"]
            }),
        },
        ToolDefinition {
            name: UPDATE_MEMORY_TOOL.to_string(),
            description: "Replace a previously remembered fact that is now outdated or wrong. \
                          Find it by an unambiguous substring of its exact wording; if more than \
                          one remembered fact matches, this fails and you should use a longer, \
                          more specific substring."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "matching": {
                        "type": "string",
                        "description": "A substring that uniquely identifies the fact to replace, e.g. \"bun over node\"."
                    },
                    "fact": {
                        "type": "string",
                        "description": "The corrected fact to store in its place."
                    }
                },
                "required": ["matching", "fact"]
            }),
        },
        ToolDefinition {
            name: FORGET_TOOL.to_string(),
            description: "Permanently delete a previously remembered fact, found by an \
                          unambiguous substring of its exact wording (same matching rule as \
                          update_memory)."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "matching": {
                        "type": "string",
                        "description": "A substring that uniquely identifies the fact to delete."
                    }
                },
                "required": ["matching"]
            }),
        },
    ]
}

/// Reads a UTF-8 text file from within the project, reusing the shared path-safety checks.
struct ReadFileTool {
    root: PathBuf,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description:
                "Read a UTF-8 text file from the project. Prefer this for reading file contents \
                          instead of using run_command with shell readers such as cat or sed. The \
                          path must be relative to the project root, for example src/main.rs."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Project-relative path to the file to read."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let path = value
            .get("path")
            .and_then(|path| path.as_str())
            .context("read_file requires a 'path' string argument")?;
        read_project_file(&self.root, path)
    }
}

/// Image execution is handled by the chat loop because ordinary tool results are text-only.
struct ReadImageTool;

#[async_trait]
impl Tool for ReadImageTool {
    fn name(&self) -> &str {
        "read_image"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Read an image from the project and attach it as visual input for the model. Requires a vision-capable model.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Project-relative path to a PNG, JPEG, WebP, or GIF image." },
                    "detail": { "type": "string", "enum": ["low", "high", "auto"], "default": "auto" }
                },
                "required": ["path"]
            }),
        }
    }
    async fn run(&self, _arguments: &str) -> Result<String> {
        anyhow::bail!("read_image must be executed by the multimodal chat loop")
    }
}

/// Lists the entries of a directory within the project, so the model can discover files to read.
struct ListDirectoryTool {
    root: PathBuf,
}

#[async_trait]
impl Tool for ListDirectoryTool {
    fn name(&self) -> &str {
        "list_directory"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "List the entries of a directory in the project. The path must be relative \
                          to the project root; use \".\" for the root. Directories are shown with a \
                          trailing slash."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Project-relative directory path, e.g. src or \".\" for the root."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let path = value
            .get("path")
            .and_then(|path| path.as_str())
            .context("list_directory requires a 'path' string argument")?;
        list_project_directory(&self.root, path)
    }
}

/// Render a path relative to the project root using forward slashes, so glob patterns and output
/// are platform-independent (on Windows, `Path::display`/`to_string_lossy` otherwise render `\`).
/// `pub(crate)` so `/index` (`chat::run_index`) can reuse it for chunk paths.
pub(crate) fn relative_slug(root: &Path, entry: &Path) -> String {
    entry
        .strip_prefix(root)
        .unwrap_or(entry)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Walk a project-relative scope (a file or a directory), honouring `.gitignore` with the same
/// flags `read_project_directory` in `context.rs` uses for `@dir` expansion, so ignore behavior is
/// consistent across every tool that walks the tree. `pub(crate)` so `/index` (`chat::run_index`)
/// can walk the whole project the same way `grep`/`glob` do.
pub(crate) fn walk(scope: &Path) -> impl Iterator<Item = PathBuf> {
    ignore::WalkBuilder::new(scope)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .build()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(|entry| entry.into_path())
}

/// Target line-window size `/index` aims for when it splits a file. Natural boundaries may move a
/// chunk edge a little earlier or later, but files with no useful boundary keep this exact window.
pub(crate) const CHUNK_LINES: usize = 50;
const CHUNK_MIN_LINES: usize = 20;
const CHUNK_MAX_LINES: usize = 80;

/// Split file content into 1-indexed line-number windows for `/index` to embed. It prefers blank
/// lines and common declaration starts near the target size so search results tend to return whole
/// symbols instead of cutting through them. Empty content yields no chunks. `pub(crate)` so
/// `chat::run_index` can use it.
pub(crate) fn chunk_text(content: &str) -> Vec<(usize, usize, String)> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let remaining = lines.len() - start;
        let length = if remaining <= CHUNK_LINES {
            remaining
        } else {
            preferred_chunk_length(&lines, start).unwrap_or(CHUNK_LINES)
        };
        let end = start + length;
        chunks.push((start + 1, end, lines[start..end].join("\n")));
        start = end;
    }
    chunks
}

fn preferred_chunk_length(lines: &[&str], start: usize) -> Option<usize> {
    let min = start + CHUNK_MIN_LINES;
    let target = (start + CHUNK_LINES).min(lines.len());
    let max = (start + CHUNK_MAX_LINES).min(lines.len());

    find_blank_boundary(lines, min, target, max)
        .or_else(|| find_declaration_boundary(lines, min, target, max))
        .map(|end| end - start)
}

fn find_blank_boundary(lines: &[&str], min: usize, target: usize, max: usize) -> Option<usize> {
    find_boundary_end(min, target, max, |end| lines[end - 1].trim().is_empty())
}

fn find_declaration_boundary(
    lines: &[&str],
    min: usize,
    target: usize,
    max: usize,
) -> Option<usize> {
    find_boundary_end(min, target, max, |end| {
        looks_like_declaration_start(lines[end].trim_start())
    })
}

fn find_boundary_end(
    min: usize,
    target: usize,
    max: usize,
    matches: impl Fn(usize) -> bool,
) -> Option<usize> {
    (target..max)
        .find(|&end| matches(end))
        .or_else(|| (min..target).rev().find(|&end| matches(end)))
}

fn looks_like_declaration_start(line: &str) -> bool {
    const PREFIXES: [&str; 28] = [
        "async fn ",
        "class ",
        "const ",
        "def ",
        "enum ",
        "export class ",
        "export const ",
        "export default function ",
        "export enum ",
        "export function ",
        "export interface ",
        "export type ",
        "fn ",
        "function ",
        "impl ",
        "interface ",
        "mod ",
        "pub async fn ",
        "pub const ",
        "pub enum ",
        "pub fn ",
        "pub mod ",
        "pub struct ",
        "pub trait ",
        "struct ",
        "trait ",
        "type ",
        "use ",
    ];
    PREFIXES.iter().any(|prefix| line.starts_with(prefix))
}

/// Name of the pseudo-tool that answers a natural-language query against `/index`'s embedding
/// store. Intercepted by the chat loop the same way `ask_user`/`spawn_agent` are: it needs the
/// active `Provider`/embedding model and `Database`, which `Tool::run` has no way to receive. Not
/// unconditionally included in `ToolRegistry::definitions()` — the chat loop only offers it when
/// the active profile has an `embedding_model` configured (see `search_code_definition`).
pub const SEARCH_CODE_TOOL: &str = "search_code";

/// Definition for `search_code`, pushed onto the tool list by the chat loop only when the active
/// profile's `embedding_model` is configured — see `SEARCH_CODE_TOOL`.
pub fn search_code_definition() -> ToolDefinition {
    ToolDefinition {
        name: SEARCH_CODE_TOOL.to_string(),
        description: "Search the project's indexed code by meaning (not literal text) for a \
                      natural-language query, e.g. \"where do we validate the api key\". \
                      Returns the most relevant chunks as path:start-end with their text; read \
                      the file for full context. Requires the user to have run /index first — if \
                      results seem stale or empty, ask them to re-run it. Prefer grep for an \
                      exact string or symbol name; use this when you don't know the exact wording."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "A natural-language description of what to find."
                }
            },
            "required": ["query"]
        }),
    }
}

/// Searches file contents in the project for a regular expression. Read-only, so it never requires
/// confirmation.
struct GrepTool {
    root: PathBuf,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search file contents in the project for a regular expression. Respects \
                          .gitignore. Prefer this over run_command with shell grep/find for \
                          locating code."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression to search for, e.g. \"fn\\s+main\"."
                    },
                    "path": {
                        "type": "string",
                        "description": "Project-relative directory or file to search within. Defaults to the project root."
                    },
                    "glob": {
                        "type": "string",
                        "description": "Only search files whose project-relative path matches this glob, e.g. \"*.rs\"."
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "Match case-insensitively. Defaults to false."
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let pattern = value
            .get("pattern")
            .and_then(|pattern| pattern.as_str())
            .context("grep requires a 'pattern' string argument")?;
        let path = value
            .get("path")
            .and_then(|path| path.as_str())
            .unwrap_or(".");
        let glob = value.get("glob").and_then(|glob| glob.as_str());
        let case_insensitive = value
            .get("case_insensitive")
            .and_then(|flag| flag.as_bool())
            .unwrap_or(false);

        let scope = resolve_within_root(&self.root, path)?;
        let matcher = glob
            .map(|pattern| {
                globset::Glob::new(pattern)
                    .with_context(|| format!("invalid glob pattern: {pattern}"))
                    .map(|glob| glob.compile_matcher())
            })
            .transpose()?;
        let regex = regex::RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
            .with_context(|| format!("invalid regular expression: {pattern}"))?;

        let mut results = Vec::new();
        let mut omitted = 0usize;
        for entry in walk(&scope) {
            let relative = relative_slug(&self.root, &entry);
            if let Some(matcher) = &matcher
                && !matcher.is_match(&relative)
            {
                continue;
            }
            if std::fs::metadata(&entry)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
                > MAX_SEARCH_FILE_BYTES
            {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&entry) else {
                continue;
            };
            for (number, line) in content.lines().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                if results.len() >= MAX_SEARCH_MATCHES {
                    omitted += 1;
                    continue;
                }
                results.push(format!("{}:{}: {}", relative, number + 1, line.trim()));
            }
        }

        if results.is_empty() {
            return Ok("no matches".to_string());
        }
        let mut output = results.join("\n");
        if omitted > 0 {
            output.push_str(&format!("\n… {omitted} more matches omitted"));
        }
        Ok(cap(&output, MAX_SEARCH_OUTPUT))
    }
}

/// Finds project files by a glob pattern. Read-only, so it never requires confirmation.
struct GlobTool {
    root: PathBuf,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Find project files by a glob pattern, e.g. \"src/**/*.rs\". Respects \
                          .gitignore. Prefer this over run_command with shell find/ls for locating \
                          files."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match against project-relative paths, e.g. \"src/**/*.rs\"."
                    },
                    "path": {
                        "type": "string",
                        "description": "Project-relative directory to search within. Defaults to the project root."
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let pattern = value
            .get("pattern")
            .and_then(|pattern| pattern.as_str())
            .context("glob requires a 'pattern' string argument")?;
        let path = value
            .get("path")
            .and_then(|path| path.as_str())
            .unwrap_or(".");

        let scope = resolve_within_root(&self.root, path)?;
        let matcher = globset::Glob::new(pattern)
            .with_context(|| format!("invalid glob pattern: {pattern}"))?
            .compile_matcher();

        let mut matches: Vec<String> = walk(&scope)
            .filter_map(|entry| {
                let relative = relative_slug(&self.root, &entry);
                matcher.is_match(&relative).then_some(relative)
            })
            .collect();
        matches.sort();

        if matches.is_empty() {
            return Ok("no matches".to_string());
        }
        let total = matches.len();
        if total > MAX_SEARCH_MATCHES {
            matches.truncate(MAX_SEARCH_MATCHES);
            matches.push(format!(
                "… {} more matches omitted",
                total - MAX_SEARCH_MATCHES
            ));
        }
        Ok(matches.join("\n"))
    }
}

/// Name of the `update_plan` tool, exposed so the chat loop can recognize it and render its
/// arguments as a checklist instead of the generic trace line (see `render_plan`).
pub const UPDATE_PLAN_TOOL: &str = "update_plan";

/// A single step in a model-declared plan for a multi-step task, shown to the user as a checklist.
#[derive(serde::Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: PlanStepStatus,
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

fn parse_plan(arguments: &str) -> Result<Vec<PlanStep>> {
    #[derive(serde::Deserialize)]
    struct Arguments {
        plan: Vec<PlanStep>,
    }
    let arguments: Arguments =
        serde_json::from_str(arguments).context("update_plan requires a 'plan' array argument")?;
    let in_progress = arguments
        .plan
        .iter()
        .filter(|step| step.status == PlanStepStatus::InProgress)
        .count();
    if in_progress > 1 {
        anyhow::bail!("only one step may be in_progress at a time, found {in_progress}");
    }
    Ok(arguments.plan)
}

/// Structured plan data for lightweight UI mirrors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanView {
    pub steps: Vec<(String, PlanStepStatus)>,
    pub active: Option<String>,
}

pub fn plan_view(arguments: &str) -> Option<PlanView> {
    let steps = parse_plan(arguments).ok()?;
    let active = steps
        .iter()
        .find_map(|step| (step.status == PlanStepStatus::InProgress).then(|| step.step.clone()));
    Some(PlanView {
        steps: steps.into_iter().map(|s| (s.step, s.status)).collect(),
        active,
    })
}

/// Number of steps in an `update_plan` call, or `None` if it doesn't parse.
pub fn plan_step_count(arguments: &str) -> Option<usize> {
    parse_plan(arguments).ok().map(|plan| plan.len())
}

/// Render an `update_plan` call's raw arguments as a checklist for the chat trace, matching the
/// existing 4-space-indent trace convention. Returns `None` if the arguments don't parse, so the
/// caller can fall back to the generic `  → name(args)` trace line.
pub fn render_plan(arguments: &str) -> Option<String> {
    let plan = parse_plan(arguments).ok()?;
    if plan.is_empty() {
        return Some("    (empty plan)".to_string());
    }
    Some(
        plan.iter()
            .map(|step| {
                let mark = match step.status {
                    PlanStepStatus::Completed => "x",
                    PlanStepStatus::InProgress => "~",
                    PlanStepStatus::Pending => " ",
                };
                format!("    [{mark}] {}", step.step)
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Lets the model declare or replace the checklist for a multi-step task. It has no side effects of
/// its own outside the conversation — it only validates the plan's shape and hands it back; the
/// chat loop renders it specially (see `render_plan`) instead of the generic trace line.
struct UpdatePlanTool;

#[async_trait]
impl Tool for UpdatePlanTool {
    fn name(&self) -> &str {
        UPDATE_PLAN_TOOL
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Declare or replace the checklist for a multi-step task, shown to the \
                          user as a live plan. Send the full list every time, not just changed \
                          steps. Mark at most one step in_progress at a time. Use this for tasks \
                          with three or more distinct steps; skip it for simple, single-step \
                          requests."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": {
                                    "type": "string",
                                    "description": "A short description of this step."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["step", "status"]
                        }
                    }
                },
                "required": ["plan"]
            }),
        }
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let plan = parse_plan(arguments)?;
        Ok(format!("ok ({} steps)", plan.len()))
    }
}

/// Timeout policy for `run_command`, from `kamui.toml`'s `[commands]` section (see
/// `config::Config::command_timeout_secs`/`background_max_secs`). `timeout` bounds a foreground
/// command; `background_max` bounds how long a `background: true` job may run before Kamui kills
/// it as a safety net against runaway/zombie processes — not a limit meant to constrain a
/// legitimately long-running command, which should just be run in the background instead.
#[derive(Debug, Clone, Copy)]
pub struct CommandLimits {
    pub timeout: Duration,
    pub background_max: Duration,
}

impl Default for CommandLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            background_max: Duration::from_secs(30 * 60),
        }
    }
}

/// A background job's status. `Copy` so a snapshot can be read out from behind its mutex without
/// holding the lock any longer than necessary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Running,
    Exited(i32),
    Killed,
    TimedOut,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobStatus::Running => write!(formatter, "running"),
            JobStatus::Exited(code) => write!(formatter, "exited ({code})"),
            JobStatus::Killed => write!(formatter, "killed"),
            JobStatus::TimedOut => write!(formatter, "timed out"),
        }
    }
}

/// Mutable state of one background job: updated once by its draining task when the process ends,
/// read at any time by `command_status`/`/jobs`. Output is captured in full and only capped at the
/// end, so a killed or timed-out job still reports whatever it had produced.
struct JobState {
    status: JobStatus,
    stdout: String,
    stderr: String,
}

/// One tracked background job: identity that never changes after creation, a kill switch, and its
/// mutable state. `pub(crate)` only because it appears in the public `JobRegistry` alias; nothing
/// outside this module constructs or matches on it directly.
pub(crate) struct JobEntry {
    command: String,
    started_at: Instant,
    kill: tokio::sync::watch::Sender<bool>,
    state: Mutex<JobState>,
}

/// Background jobs started by `run_command(background: true)`, shared by `run_command`,
/// `command_status`, `stop_command`, and the chat loop's `/jobs` command. In-memory only: jobs do
/// not survive a restart, and any still running are killed on shutdown (see `kill_all_jobs`).
pub type JobRegistry = Arc<Mutex<HashMap<String, Arc<JobEntry>>>>;

/// Jobs that have finished since the last call, as one line each. A background job could only
/// be discovered by polling `/jobs` or by the model calling `command_status`, so a `cargo test`
/// started in the background could finish minutes ago and say nothing. `announced` is the
/// caller's record of what it has already reported; ids are added to it here.
pub fn drain_finished_jobs(
    jobs: &JobRegistry,
    announced: &mut std::collections::HashSet<String>,
) -> Vec<String> {
    let registry = jobs.lock().unwrap();
    let mut finished: Vec<(String, String)> = registry
        .iter()
        .filter_map(|(id, entry)| {
            let status = entry.state.lock().unwrap().status;
            if status == JobStatus::Running || announced.contains(id) {
                return None;
            }
            Some((
                id.clone(),
                format!(
                    "job {id} {status} after {}s: {}",
                    entry.started_at.elapsed().as_secs(),
                    entry.command
                ),
            ))
        })
        .collect();
    // Sorted for a stable order; a HashMap yields a different one each time.
    finished.sort_by(|left, right| left.0.cmp(&right.0));
    finished
        .into_iter()
        .map(|(id, line)| {
            announced.insert(id);
            line
        })
        .collect()
}

/// List every background job as one line each (`id  status  elapsed  command`), sorted by id for a
/// stable order. Shared by `CommandStatusTool` (no `job_id` given) and the chat loop's `/jobs`.
pub fn describe_jobs(jobs: &JobRegistry) -> String {
    let registry = jobs.lock().unwrap();
    if registry.is_empty() {
        return "no background jobs".to_string();
    }
    let mut lines: Vec<(String, String)> = registry
        .iter()
        .map(|(id, entry)| {
            let status = entry.state.lock().unwrap().status;
            (
                id.clone(),
                format!(
                    "{id}  {status}  {}s  {}",
                    entry.started_at.elapsed().as_secs(),
                    entry.command
                ),
            )
        })
        .collect();
    lines.sort_by(|a, b| a.0.cmp(&b.0));
    lines
        .into_iter()
        .map(|(_, line)| line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Signal every tracked job to stop, ignoring jobs that already finished (their receiver is gone,
/// so `send` errors harmlessly). Called on shutdown so nothing outlives the Kamui process.
pub fn kill_all_jobs(jobs: &JobRegistry) {
    let registry = jobs.lock().unwrap();
    for entry in registry.values() {
        let _ = entry.kill.send(true);
    }
}

/// Run one background job to completion, updating `entry` when it finishes. Spawned detached by
/// `RunCommandTool::run` — nothing awaits this directly.
async fn run_background_job(
    executed: String,
    root: PathBuf,
    background_max: Duration,
    entry: Arc<JobEntry>,
    mut kill_rx: tokio::sync::watch::Receiver<bool>,
) {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut child = match tokio::process::Command::new(shell)
        .arg(flag)
        .arg(&executed)
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let mut state = entry.state.lock().unwrap();
            state.status = JobStatus::Exited(-1);
            state.stderr = format!("failed to start the command: {error:#}");
            return;
        }
    };
    // Owning the pipes separately from `child` means their readers keep running (and keep
    // whatever they already captured) even if the process below is killed or times out.
    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stdout_pipe, &mut buffer).await;
        buffer
    });
    let stderr_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr_pipe, &mut buffer).await;
        buffer
    });

    enum Outcome {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Killed,
    }
    let outcome = tokio::select! {
        result = child.wait() => Outcome::Exited(result),
        () = tokio::time::sleep(background_max) => Outcome::TimedOut,
        _ = kill_rx.changed() => Outcome::Killed,
    };
    let status = match outcome {
        Outcome::Exited(Ok(exit_status)) => JobStatus::Exited(exit_status.code().unwrap_or(-1)),
        Outcome::Exited(Err(_)) => JobStatus::Exited(-1),
        Outcome::TimedOut => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            JobStatus::TimedOut
        }
        Outcome::Killed => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            JobStatus::Killed
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();
    let mut state = entry.state.lock().unwrap();
    state.status = status;
    state.stdout = cap(&String::from_utf8_lossy(&stdout), MAX_COMMAND_OUTPUT);
    state.stderr = cap(&String::from_utf8_lossy(&stderr), MAX_COMMAND_OUTPUT);
}

/// Runs a shell command in the project directory, either waiting for it (default) or starting it
/// as a background job. This tool has side effects, so it requires user confirmation (enforced by
/// the chat loop) and is bounded by a timeout and an output cap. `allow_commands` is an exact-match
/// allowlist (from the global `[permissions]` config, see `config::Config::allow_commands`) of
/// commands that skip confirmation entirely.
struct RunCommandTool {
    root: PathBuf,
    allow_commands: Vec<String>,
    timeout: Duration,
    background_max: Duration,
    jobs: JobRegistry,
}

/// Whether the external `rtk` binary is available. Detected once per process; RTK is an optional
/// output-compression backend, never a requirement.
fn rtk_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("rtk")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// Names the shell runs itself instead of executing a program of that name. `run_command` hands the
/// whole string to `cmd /C` or `sh -c`, so its first word is not necessarily a program — but `rtk`
/// can only prefix a program, so routing one of these turns a working command into
/// `rtk: Binary 'cd' not found on PATH`.
///
/// Which names these are is the shell's decision, not the platform's, which is why the lists are
/// per-shell and only the one actually invoked is consulted: `echo` is a `cmd` builtin on Windows
/// but a real `/bin/echo` on Unix. `echo` is also why the Windows list must not be narrowed to
/// "names with no binary anywhere" — an unrelated `echo.exe` on `PATH` (MSYS, or a stray one from
/// some bundled toolchain) makes `rtk echo "hi"` succeed while silently printing something `cmd`'s
/// own `echo` would not, so the shell's builtin has to win regardless of what is installed.
const CMD_BUILTINS: &[&str] = &[
    "assoc", "break", "call", "cd", "chdir", "cls", "color", "copy", "date", "del", "dir", "echo",
    "endlocal", "erase", "exit", "for", "ftype", "goto", "if", "md", "mkdir", "mklink", "move",
    "path", "pause", "popd", "prompt", "pushd", "rd", "rem", "ren", "rename", "rmdir", "set",
    "setlocal", "shift", "start", "time", "title", "type", "ver", "verify", "vol",
];
const SH_BUILTINS: &[&str] = &[
    ".", ":", "alias", "bg", "break", "cd", "command", "continue", "eval", "exec", "exit",
    "export", "fg", "getopts", "hash", "jobs", "local", "read", "readonly", "return", "set",
    "shift", "source", "times", "trap", "ulimit", "umask", "unalias", "unset", "wait",
];

/// Decide whether to route a command through `rtk`. Only simple commands are routed: with shell
/// operators the prefix would apply to the first segment only, so those run directly. Commands the
/// model already prefixed with `rtk` are left untouched, and so is anything whose first word the
/// shell would not resolve to a program — a builtin, or a `VAR=value` assignment prefix.
fn route_through_rtk(command: &str, rtk_is_available: bool) -> bool {
    if !rtk_is_available {
        return false;
    }
    let trimmed = command.trim();
    if trimmed == "rtk" || trimmed.starts_with("rtk ") {
        return false;
    }
    const SHELL_OPERATORS: [char; 9] = ['&', '|', ';', '>', '<', '`', '$', '(', '\n'];
    if trimmed.contains(SHELL_OPERATORS) {
        return false;
    }
    let Some(program) = trimmed.split_whitespace().next() else {
        return false;
    };
    // `RUST_LOG=debug cargo test` opens with an assignment the shell consumes, not a program name,
    // so there is nothing for `rtk` to prefix. A builtin list cannot catch this — the name is
    // arbitrary. Declining is deliberate rather than inserting `rtk` after the assignments: that
    // would rewrite the middle of the user's approved command for a marginal gain, and `cmd` has no
    // inline-assignment syntax at all, so on Windows the string is not a runnable command either way.
    if program.contains('=') {
        return false;
    }
    let (builtins, program) = if cfg!(windows) {
        // `cmd` matches its builtins case-insensitively; `sh` does not.
        (CMD_BUILTINS, program.to_ascii_lowercase())
    } else {
        (SH_BUILTINS, program.to_owned())
    };
    !builtins.contains(&program.as_str())
}

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    /// Exempt an exact allowlisted command (see `RunCommandTool::allow_commands`) from
    /// confirmation. Unparsable arguments fall back to requiring confirmation — `dispatch` will
    /// surface the actual JSON error to the model either way.
    fn requires_confirmation_for(&self, arguments: &str) -> bool {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
            return true;
        };
        let Some(command) = value.get("command").and_then(|command| command.as_str()) else {
            return true;
        };
        !self
            .allow_commands
            .iter()
            .any(|allowed| allowed.trim() == command.trim())
    }

    fn preview(&self, arguments: &str) -> Option<String> {
        let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
        let command = value.get("command")?.as_str()?;
        let background = value
            .get("background")
            .and_then(|flag| flag.as_bool())
            .unwrap_or(false);
        Some(if background {
            format!("    $ {command}  (background)")
        } else {
            format!("    $ {command}")
        })
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description:
                "Run a shell command in the project directory and return its exit code and \
                          output. The user must approve each command before it runs. Use it for \
                          builds and tests. Pass background: true for a command that runs longer \
                          than a normal foreground timeout (e.g. a dev server or a slow test \
                          suite) — it returns a job id immediately instead of waiting; check on it \
                          with command_status and stop it early with stop_command."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to run, e.g. cargo test."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Run without waiting for it to finish. Defaults to false."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let command = value
            .get("command")
            .and_then(|command| command.as_str())
            .context("run_command requires a 'command' string argument")?;
        let background = value
            .get("background")
            .and_then(|flag| flag.as_bool())
            .unwrap_or(false);

        // Route supported invocations through the optional rtk binary to compress output before it
        // reaches model context; everything else runs the command directly.
        let executed = if route_through_rtk(command, rtk_available()) {
            format!("rtk {}", command.trim())
        } else {
            command.to_string()
        };

        if background {
            let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
            let (kill_tx, kill_rx) = tokio::sync::watch::channel(false);
            let entry = Arc::new(JobEntry {
                command: command.to_string(),
                started_at: Instant::now(),
                kill: kill_tx,
                state: Mutex::new(JobState {
                    status: JobStatus::Running,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            });
            self.jobs.lock().unwrap().insert(id.clone(), entry.clone());
            tokio::spawn(run_background_job(
                executed,
                self.root.clone(),
                self.background_max,
                entry,
                kill_rx,
            ));
            return Ok(format!("started background job {id}: {command}"));
        }

        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        let child = tokio::process::Command::new(shell)
            .arg(flag)
            .arg(&executed)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to start the command")?;

        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(result) => {
                let output = format_command_output(&result.context("failed to run the command")?);
                Ok(format!("command: {executed}\n{output}"))
            }
            Err(_) => Ok(format!(
                "Error: command timed out after {} seconds and was terminated",
                self.timeout.as_secs()
            )),
        }
    }
}

/// Reports on a job started by `run_command(background: true)`. Read-only, so it never requires
/// confirmation.
struct CommandStatusTool {
    jobs: JobRegistry,
}

#[async_trait]
impl Tool for CommandStatusTool {
    fn name(&self) -> &str {
        "command_status"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Check on a background job started by run_command(background: true). \
                          Omit job_id to list every job; pass it to see that job's status and \
                          captured output. When the job is running, this waits up to 30 seconds \
                          for it to finish before returning, so do not poll repeatedly."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "The job id returned when the background command was started. Omit to list all jobs."
                    }
                }
            }),
        }
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let job_id = value.get("job_id").and_then(|id| id.as_str());

        let Some(job_id) = job_id else {
            return Ok(describe_jobs(&self.jobs));
        };
        let entry = self
            .jobs
            .lock()
            .unwrap()
            .get(job_id)
            .cloned()
            .with_context(|| format!("no background job '{job_id}'"))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if entry.state.lock().unwrap().status != JobStatus::Running
                || Instant::now() >= deadline
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let state = entry.state.lock().unwrap();
        let mut body = format!(
            "job {job_id}: {}\nstatus: {}\nelapsed: {}s",
            entry.command,
            state.status,
            entry.started_at.elapsed().as_secs()
        );
        if !state.stdout.trim().is_empty() {
            body.push_str("\nstdout:\n");
            body.push_str(state.stdout.trim_end());
        }
        if !state.stderr.trim().is_empty() {
            body.push_str("\nstderr:\n");
            body.push_str(state.stderr.trim_end());
        }
        Ok(body)
    }
}

/// Stops a job started by `run_command(background: true)`. Killing a process the user already
/// approved starting is cleanup, not a new capability, so it never requires confirmation.
struct StopCommandTool {
    jobs: JobRegistry,
}

#[async_trait]
impl Tool for StopCommandTool {
    fn name(&self) -> &str {
        "stop_command"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Stop a background job started by run_command(background: true)."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "The job id returned when the background command was started."
                    }
                },
                "required": ["job_id"]
            }),
        }
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let value: serde_json::Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let job_id = value
            .get("job_id")
            .and_then(|id| id.as_str())
            .context("stop_command requires a 'job_id' string argument")?;

        let registry = self.jobs.lock().unwrap();
        let entry = registry
            .get(job_id)
            .with_context(|| format!("no background job '{job_id}'"))?;
        let status = entry.state.lock().unwrap().status;
        if status != JobStatus::Running {
            return Ok(format!("job {job_id} is already {status}"));
        }
        let _ = entry.kill.send(true);
        Ok(format!("stop requested for job {job_id}"))
    }
}

/// Name of the `patch_file` tool, exposed so the chat loop can recognize it and snapshot its
/// target before dispatch (see `patch_target`), enabling revert-on-cancel for multi-file edits.
pub const PATCH_FILE_TOOL: &str = "patch_file";

/// Applies an exact-match text replacement to one project file, or creates a new file. Mutating,
/// so it requires user confirmation, and it previews the change as +/- lines before approval.
struct PatchFileTool {
    root: PathBuf,
}

/// Resolve a `patch_file` call's target path without applying it, so the chat loop can snapshot
/// the file before dispatching an approved patch and revert it if the turn is later cancelled.
/// Returns `None` if the arguments don't parse or the path cannot be resolved.
pub fn patch_target(root: &Path, arguments: &str) -> Option<PathBuf> {
    let patch = parse_patch_arguments(arguments).ok()?;
    resolve_for_write(root, &patch.path).ok()
}

struct PatchArguments {
    path: String,
    old_text: String,
    new_text: String,
}

fn parse_patch_arguments(arguments: &str) -> Result<PatchArguments> {
    let value: serde_json::Value =
        serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
    let path = value
        .get("path")
        .and_then(|path| path.as_str())
        .context("patch_file requires a 'path' string argument")?;
    let new_text = value
        .get("new_text")
        .and_then(|text| text.as_str())
        .context("patch_file requires a 'new_text' string argument")?;
    let old_text = value
        .get("old_text")
        .and_then(|text| text.as_str())
        .unwrap_or_default();
    Ok(PatchArguments {
        path: path.to_string(),
        old_text: old_text.to_string(),
        new_text: new_text.to_string(),
    })
}

fn line_trimmed_match(haystack: &str, needle: &str) -> std::result::Result<(usize, usize), usize> {
    let needle_lines: Vec<&str> = needle.lines().map(str::trim).collect();
    if needle_lines.is_empty() {
        return Err(0);
    }
    let mut starts = Vec::new();
    let mut offset = 0usize;
    let lines: Vec<(&str, usize, usize, usize)> = haystack
        .split_inclusive('\n')
        .map(|line| {
            let start = offset;
            offset += line.len();
            let content = line.trim_end_matches('\n');
            (content, start, start + content.len(), offset)
        })
        .collect();
    for window in lines.windows(needle_lines.len()) {
        if window
            .iter()
            .zip(&needle_lines)
            .all(|((line, _, _, _), needle)| line.trim() == *needle)
        {
            let last = window.last().expect("window is non-empty");
            starts.push((
                window[0].1,
                if needle.ends_with('\n') {
                    last.3
                } else {
                    last.2
                },
            ));
        }
    }
    if starts.len() == 1 {
        Ok(starts[0])
    } else {
        Err(starts.len())
    }
}

#[async_trait]
impl Tool for PatchFileTool {
    fn name(&self) -> &str {
        PATCH_FILE_TOOL
    }

    fn requires_confirmation(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Modify one project file by replacing old_text (which must match the file \
                          exactly once) with new_text. To create a new file, pass an empty old_text \
                          and the full content as new_text. The user must approve each patch. Read \
                          the file first so old_text matches exactly."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Project-relative path of the file to modify or create."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Exact text to replace. Must appear exactly once. Empty to create a new file."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Replacement text, or the full content of a new file."
                    }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        }
    }

    fn preview(&self, arguments: &str) -> Option<String> {
        let patch = parse_patch_arguments(arguments).ok()?;
        let mut lines = vec![format!("    --- {}", patch.path)];
        for line in patch.old_text.lines() {
            lines.push(format!("    - {line}"));
        }
        for line in patch.new_text.lines() {
            lines.push(format!("    + {line}"));
        }
        const MAX_PREVIEW_LINES: usize = 40;
        if lines.len() > MAX_PREVIEW_LINES {
            let hidden = lines.len() - MAX_PREVIEW_LINES;
            lines.truncate(MAX_PREVIEW_LINES);
            lines.push(format!("    … ({hidden} more lines)"));
        }
        Some(lines.join("\n"))
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let patch = parse_patch_arguments(arguments)?;
        let target = resolve_for_write(&self.root, &patch.path)?;

        if patch.old_text.is_empty() {
            if target.exists() {
                anyhow::bail!(
                    "{} already exists; provide old_text to modify it",
                    patch.path
                );
            }
            write_atomic(&target, &patch.new_text)?;
            return Ok(format!(
                "created {} ({} bytes)",
                patch.path,
                patch.new_text.len()
            ));
        }

        let current = read_project_file(&self.root, &patch.path)?;
        // Match line-ending-agnostically: files (especially on Windows) often use CRLF, but models
        // emit LF, so a raw byte match would fail spuriously. Compare in LF space and restore the
        // file's line-ending style on write.
        let uses_crlf = current.contains("\r\n");
        let normalized = current.replace("\r\n", "\n");
        let old_text = patch.old_text.replace("\r\n", "\n");
        let new_text = patch.new_text.replace("\r\n", "\n");

        let updated = match normalized.matches(&old_text).count() {
            0 => match line_trimmed_match(&normalized, &old_text) {
                Ok((start, end)) => {
                    format!("{}{}{}", &normalized[..start], new_text, &normalized[end..])
                }
                Err(0) => anyhow::bail!(
                    "old_text was not found in {}; read the file again and copy the text exactly",
                    patch.path
                ),
                Err(matches) => anyhow::bail!(
                    "old_text matches {matches} line-trimmed blocks in {}; include more surrounding context",
                    patch.path
                ),
            },
            1 => normalized.replacen(&old_text, &new_text, 1),
            occurrences => anyhow::bail!(
                "old_text appears {occurrences} times in {}; include more surrounding context so it matches exactly once",
                patch.path
            ),
        };
        let updated = if uses_crlf {
            updated.replace('\n', "\r\n")
        } else {
            updated
        };
        write_atomic(&target, &updated)?;
        Ok(format!("patched {}", patch.path))
    }
}

/// Write through a temporary file in the same directory and rename it into place, so an interrupted
/// write can never leave a half-written file behind. `pub(crate)` so the chat loop can reuse it to
/// revert a file to a pre-turn snapshot (see `chat::revert_snapshot`).
pub(crate) fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let mut temp_name = path
        .file_name()
        .context("write target has no file name")?
        .to_os_string();
    temp_name.push(".kamui-tmp");
    let temp = path.with_file_name(temp_name);
    std::fs::write(&temp, content)
        .with_context(|| format!("failed to write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| {
        let _ = std::fs::remove_file(&temp);
        format!("failed to replace {}", path.display())
    })
}

/// Runs a user-typed `!<command>` directly in the project shell. No approval flow — the human
/// typed it — but the same timeout, output cap, and exit-code reporting as the model's
/// `run_command` tool. Returns the formatted transcript text.
pub async fn run_direct_command(
    root: &std::path::Path,
    command: &str,
    timeout: std::time::Duration,
) -> String {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let child = tokio::process::Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) => return format!("Error: failed to start the command: {error:#}"),
    };
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => match result {
            Ok(output) => format!("command: {command}\n{}", format_command_output(&output)),
            Err(error) => format!("Error: failed to run the command: {error:#}"),
        },
        Err(_) => format!(
            "Error: command timed out after {} seconds and was terminated",
            timeout.as_secs()
        ),
    }
}

fn format_command_output(output: &Output) -> String {
    let code = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "unknown (terminated by signal)".to_string());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut body = format!("exit code: {code}");
    if !stdout.trim().is_empty() {
        body.push_str("\nstdout:\n");
        body.push_str(stdout.trim_end());
    }
    if !stderr.trim().is_empty() {
        body.push_str("\nstderr:\n");
        body.push_str(stderr.trim_end());
    }
    cap(&body, MAX_COMMAND_OUTPUT)
}

/// Truncate to at most `max` bytes on a char boundary, noting when output was cut.
fn cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (output truncated)", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registers one job directly in the registry with a chosen status.
    fn job(registry: &JobRegistry, id: &str, status: JobStatus) {
        let (kill, _) = tokio::sync::watch::channel(false);
        registry.lock().unwrap().insert(
            id.to_string(),
            Arc::new(JobEntry {
                command: format!("cargo test {id}"),
                started_at: Instant::now(),
                kill,
                state: Mutex::new(JobState {
                    status,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            }),
        );
    }

    #[test]
    fn finished_jobs_are_announced_once_each() {
        let registry: JobRegistry = Arc::new(Mutex::new(HashMap::new()));
        job(&registry, "a", JobStatus::Exited(0));
        job(&registry, "b", JobStatus::Running);
        let mut announced = std::collections::HashSet::new();

        let first = drain_finished_jobs(&registry, &mut announced);
        assert_eq!(first.len(), 1, "only the finished job: {first:?}");
        assert!(first[0].contains("job a"), "{first:?}");
        assert!(
            first[0].contains("exited (0)"),
            "the outcome is stated: {first:?}"
        );

        // Draining again says nothing: the same job must not be reported at every prompt.
        assert!(drain_finished_jobs(&registry, &mut announced).is_empty());

        // The job still running is announced only once it ends.
        registry.lock().unwrap()["b"].state.lock().unwrap().status = JobStatus::Killed;
        let second = drain_finished_jobs(&registry, &mut announced);
        assert_eq!(second.len(), 1);
        assert!(second[0].contains("killed"), "{second:?}");
    }

    #[test]
    fn finished_jobs_report_in_a_stable_order() {
        // The registry is a HashMap, so without sorting the same two jobs would be announced in
        // a different order each run.
        let registry: JobRegistry = Arc::new(Mutex::new(HashMap::new()));
        for id in ["c", "a", "b"] {
            job(&registry, id, JobStatus::Exited(0));
        }
        let mut announced = std::collections::HashSet::new();
        let lines = drain_finished_jobs(&registry, &mut announced);
        let ids: Vec<&str> = lines
            .iter()
            .map(|line| line.split_whitespace().nth(1).unwrap())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }
    use std::fs;
    use uuid::Uuid;

    fn project_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!("kamui-tools-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path.canonicalize().unwrap()
    }

    #[tokio::test]
    async fn read_file_returns_file_contents() {
        let root = project_root();
        fs::write(root.join("note.txt"), "hello tools").unwrap();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"path":"note.txt"}"#.to_string(),
        };

        assert_eq!(registry.dispatch(&call).await, "hello tools");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn read_file_rejects_paths_outside_the_project() {
        let root = project_root();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: r#"{"path":"../secret.txt"}"#.to_string(),
        };

        assert!(registry.dispatch(&call).await.starts_with("Error:"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn read_file_rejects_invalid_json_arguments() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: "not json".to_string(),
        };

        assert!(registry.dispatch(&call).await.starts_with("Error:"));
    }

    #[tokio::test]
    async fn dispatch_reports_unknown_tools() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "launch_missiles".to_string(),
            arguments: "{}".to_string(),
        };

        assert!(registry.dispatch(&call).await.contains("unknown tool"));
    }

    #[tokio::test]
    async fn list_directory_shows_directories_before_files() {
        let root = project_root();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("README.md"), "x").unwrap();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "list_directory".to_string(),
            arguments: r#"{"path":"."}"#.to_string(),
        };

        let output = registry.dispatch(&call).await;
        assert!(output.contains("src/"));
        assert!(output.contains("README.md"));
        assert!(output.find("src/").unwrap() < output.find("README.md").unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn list_directory_rejects_a_file_path() {
        let root = project_root();
        fs::write(root.join("note.txt"), "x").unwrap();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "list_directory".to_string(),
            arguments: r#"{"path":"note.txt"}"#.to_string(),
        };

        assert!(registry.dispatch(&call).await.starts_with("Error:"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn chunk_text_splits_into_fixed_line_windows() {
        let content = (1..=120)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        let chunks = chunk_text(&content);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], (1, 50, chunks[0].2.clone()));
        assert!(chunks[0].2.starts_with("line 1\n"));
        assert!(chunks[0].2.ends_with("line 50"));
        assert_eq!(chunks[1].0, 51);
        assert_eq!(chunks[1].1, 100);
        // The final, partial chunk ends exactly at the last line, not padded to CHUNK_LINES.
        assert_eq!(chunks[2], (101, 120, chunks[2].2.clone()));
    }

    #[test]
    fn chunk_text_prefers_blank_line_boundaries() {
        let mut lines = (1..=35)
            .map(|line| format!("first block line {line}"))
            .collect::<Vec<_>>();
        lines.push(String::new());
        lines.extend((1..=60).map(|line| format!("second block line {line}")));

        let chunks = chunk_text(&lines.join("\n"));

        assert_eq!(chunks[0].0, 1);
        assert_eq!(chunks[0].1, 36);
        assert!(chunks[0].2.ends_with('\n'));
        assert_eq!(chunks[1].0, 37);
    }

    #[test]
    fn chunk_text_starts_declarations_in_the_next_chunk() {
        let mut lines = (1..=40)
            .map(|line| format!("first function line {line}"))
            .collect::<Vec<_>>();
        lines.push("fn second() {}".to_string());
        lines.extend((1..=60).map(|line| format!("second function line {line}")));

        let chunks = chunk_text(&lines.join("\n"));

        assert_eq!(chunks[0].0, 1);
        assert_eq!(chunks[0].1, 40);
        assert!(!chunks[0].2.contains("fn second"));
        assert_eq!(chunks[1].0, 41);
        assert!(chunks[1].2.starts_with("fn second"));
    }

    #[test]
    fn chunk_text_returns_nothing_for_empty_content() {
        assert!(chunk_text("").is_empty());
    }

    #[tokio::test]
    async fn grep_finds_matching_lines_with_path_and_line_number() {
        let root = project_root();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\nfn helper() {}\n").unwrap();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "grep".to_string(),
            arguments: r#"{"pattern":"fn\\s+main"}"#.to_string(),
        };

        let output = registry.dispatch(&call).await;
        assert!(output.contains("src/main.rs:1:"));
        assert!(!output.contains("helper"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn grep_honours_gitignore_and_glob_filter() {
        let root = project_root();
        fs::write(root.join("keep.rs"), "needle here").unwrap();
        fs::write(root.join("skip.rs"), "needle here too").unwrap();
        fs::write(root.join("skip.txt"), "needle in a text file").unwrap();
        fs::write(root.join(".gitignore"), "skip.rs\n").unwrap();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );

        let ignored = registry
            .dispatch(&ToolCall {
                id: "c1".to_string(),
                name: "grep".to_string(),
                arguments: r#"{"pattern":"needle"}"#.to_string(),
            })
            .await;
        assert!(ignored.contains("keep.rs"));
        assert!(!ignored.contains("skip.rs"));

        let filtered = registry
            .dispatch(&ToolCall {
                id: "c2".to_string(),
                name: "grep".to_string(),
                arguments: r#"{"pattern":"needle","glob":"*.rs"}"#.to_string(),
            })
            .await;
        assert!(filtered.contains("keep.rs"));
        assert!(!filtered.contains("skip.txt"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn grep_rejects_paths_outside_the_project() {
        let root = project_root();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "grep".to_string(),
            arguments: r#"{"pattern":"x","path":"../escape"}"#.to_string(),
        };

        assert!(registry.dispatch(&call).await.starts_with("Error:"));
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn glob_matches_a_pattern_and_sorts_results() {
        let root = project_root();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/b.rs"), "").unwrap();
        fs::write(root.join("src/a.rs"), "").unwrap();
        fs::write(root.join("src/a.txt"), "").unwrap();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "glob".to_string(),
            arguments: r#"{"pattern":"src/**/*.rs"}"#.to_string(),
        };

        let output = registry.dispatch(&call).await;
        assert!(output.contains("src/a.rs"));
        assert!(output.contains("src/b.rs"));
        assert!(!output.contains("a.txt"));
        assert!(output.find("a.rs").unwrap() < output.find("b.rs").unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn glob_reports_no_matches() {
        let root = project_root();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "glob".to_string(),
            arguments: r#"{"pattern":"*.nope"}"#.to_string(),
        };

        assert_eq!(registry.dispatch(&call).await, "no matches");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn update_plan_accepts_a_valid_plan() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "update_plan".to_string(),
            arguments: r#"{"plan":[{"step":"Explore","status":"completed"},{"step":"Implement","status":"in_progress"},{"step":"Test","status":"pending"}]}"#.to_string(),
        };

        assert_eq!(registry.dispatch(&call).await, "ok (3 steps)");
    }

    #[tokio::test]
    async fn update_plan_rejects_more_than_one_in_progress_step() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "update_plan".to_string(),
            arguments: r#"{"plan":[{"step":"A","status":"in_progress"},{"step":"B","status":"in_progress"}]}"#.to_string(),
        };

        assert!(registry.dispatch(&call).await.starts_with("Error:"));
    }

    #[tokio::test]
    async fn update_plan_rejects_an_invalid_status() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "update_plan".to_string(),
            arguments: r#"{"plan":[{"step":"A","status":"done"}]}"#.to_string(),
        };

        assert!(registry.dispatch(&call).await.starts_with("Error:"));
    }

    #[test]
    fn render_plan_marks_each_step_status() {
        let rendered = render_plan(
            r#"{"plan":[{"step":"Explore","status":"completed"},{"step":"Implement","status":"in_progress"},{"step":"Test","status":"pending"}]}"#,
        )
        .unwrap();

        assert!(rendered.contains("[x] Explore"));
        assert!(rendered.contains("[~] Implement"));
        assert!(rendered.contains("[ ] Test"));
    }

    #[test]
    fn render_plan_returns_none_for_invalid_arguments() {
        assert!(render_plan("not json").is_none());
    }

    #[tokio::test]
    async fn read_only_registry_offers_only_the_safe_reading_tools() {
        let root = project_root();
        fs::write(root.join("note.txt"), "hello").unwrap();
        let registry = ToolRegistry::read_only(root.clone());

        let names: Vec<String> = registry
            .tool_definitions_only()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert_eq!(names.len(), 5);
        for expected in ["read_file", "read_image", "list_directory", "grep", "glob"] {
            assert!(names.contains(&expected.to_string()), "{names:?}");
        }
        for forbidden in ["run_command", "patch_file", "update_plan", "spawn_agent"] {
            assert!(!names.contains(&forbidden.to_string()), "{names:?}");
        }

        // None of the read-only tools ever require confirmation.
        assert!(!registry.requires_confirmation_for("read_file", "{}"));
        assert!(!registry.requires_confirmation_for("grep", "{}"));

        let output = registry
            .dispatch(&ToolCall {
                id: "c1".to_string(),
                name: "read_file".to_string(),
                arguments: r#"{"path":"note.txt"}"#.to_string(),
            })
            .await;
        assert_eq!(output, "hello");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tool_definitions_only_excludes_pseudo_tools() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );

        let real: Vec<String> = registry
            .tool_definitions_only()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        let all: Vec<String> = registry
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();

        for pseudo in [
            "ask_user",
            "spawn_agent",
            "remember",
            "update_memory",
            "forget",
        ] {
            assert!(!real.contains(&pseudo.to_string()), "{real:?}");
            assert!(all.contains(&pseudo.to_string()), "{all:?}");
        }
        assert_eq!(all.len(), real.len() + 5);
    }

    #[test]
    fn only_mutating_tools_require_confirmation() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        assert!(registry.requires_confirmation_for("run_command", "{}"));
        assert!(registry.requires_confirmation_for("patch_file", "{}"));
        assert!(!registry.requires_confirmation_for("read_file", "{}"));
        assert!(!registry.requires_confirmation_for("list_directory", "{}"));
        assert!(!registry.requires_confirmation_for("unknown", "{}"));
    }

    #[test]
    fn allowlisted_commands_skip_confirmation_only_for_an_exact_match() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            vec!["git status".to_string()],
            CommandLimits::default(),
        );

        assert!(!registry.requires_confirmation_for("run_command", r#"{"command":"git status"}"#));
        assert!(
            registry
                .requires_confirmation_for("run_command", r#"{"command":"git status --short"}"#)
        );
        assert!(registry.requires_confirmation_for("run_command", r#"{"command":"git push"}"#));
    }

    #[test]
    fn requires_confirmation_for_falls_back_to_confirming_on_unparsable_arguments() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        assert!(registry.requires_confirmation_for("run_command", "not json"));
    }

    #[tokio::test]
    async fn run_command_reports_output_and_exit_code() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let call = ToolCall {
            id: "c1".to_string(),
            name: "run_command".to_string(),
            arguments: r#"{"command":"echo kamui-ok"}"#.to_string(),
        };

        let output = registry.dispatch(&call).await;
        assert!(output.starts_with("command: "));
        assert!(output.contains("exit code: 0"));
        assert!(output.contains("kamui-ok"));
    }

    #[test]
    fn command_limits_default_values() {
        let limits = CommandLimits::default();
        assert_eq!(limits.timeout, Duration::from_secs(30));
        assert_eq!(limits.background_max, Duration::from_secs(30 * 60));
    }

    /// A command that sleeps for roughly `seconds`, on whichever shell `run_command` will use.
    fn sleep_command(seconds: u64) -> String {
        if cfg!(windows) {
            format!("ping -n {} 127.0.0.1 >NUL", seconds + 1)
        } else {
            format!("sleep {seconds}")
        }
    }

    fn job_id_from(started: &str) -> String {
        started
            .strip_prefix("started background job ")
            .and_then(|rest| rest.split(':').next())
            .expect(
                "run_command(background: true) always returns 'started background job <id>: ...'",
            )
            .to_string()
    }

    /// Poll `command_status` until the job is no longer `running`, or give up after ~2.5s.
    async fn wait_for_job(registry: &ToolRegistry, job_id: &str) -> String {
        let mut status = String::new();
        for _ in 0..50 {
            status = registry
                .dispatch(&ToolCall {
                    id: "status".to_string(),
                    name: "command_status".to_string(),
                    arguments: format!(r#"{{"job_id":"{job_id}"}}"#),
                })
                .await;
            if !status.contains("status: running") {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        status
    }

    #[tokio::test]
    async fn background_job_runs_to_completion_and_reports_output() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let started = registry
            .dispatch(&ToolCall {
                id: "c1".to_string(),
                name: "run_command".to_string(),
                arguments: r#"{"command":"echo kamui-bg","background":true}"#.to_string(),
            })
            .await;
        assert!(started.starts_with("started background job "), "{started}");
        let job_id = job_id_from(&started);

        let status = wait_for_job(&registry, &job_id).await;
        assert!(status.contains("status: exited (0)"), "{status}");
        assert!(status.contains("kamui-bg"), "{status}");

        let listed = registry
            .dispatch(&ToolCall {
                id: "c2".to_string(),
                name: "command_status".to_string(),
                arguments: "{}".to_string(),
            })
            .await;
        assert!(listed.contains(&job_id), "{listed}");
    }

    #[tokio::test]
    async fn stop_command_kills_a_running_background_job() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let started = registry
            .dispatch(&ToolCall {
                id: "c1".to_string(),
                name: "run_command".to_string(),
                arguments: format!(r#"{{"command":"{}","background":true}}"#, sleep_command(30)),
            })
            .await;
        let job_id = job_id_from(&started);

        let stopped = registry
            .dispatch(&ToolCall {
                id: "c2".to_string(),
                name: "stop_command".to_string(),
                arguments: format!(r#"{{"job_id":"{job_id}"}}"#),
            })
            .await;
        assert!(stopped.starts_with("stop requested"), "{stopped}");

        let status = wait_for_job(&registry, &job_id).await;
        assert!(status.contains("status: killed"), "{status}");
    }

    #[tokio::test]
    async fn stop_command_reports_an_unknown_job() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let output = registry
            .dispatch(&ToolCall {
                id: "c1".to_string(),
                name: "stop_command".to_string(),
                arguments: r#"{"job_id":"nope"}"#.to_string(),
            })
            .await;
        assert!(output.starts_with("Error:"));
    }

    #[tokio::test]
    async fn command_status_reports_an_unknown_job() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let output = registry
            .dispatch(&ToolCall {
                id: "c1".to_string(),
                name: "command_status".to_string(),
                arguments: r#"{"job_id":"nope"}"#.to_string(),
            })
            .await;
        assert!(output.starts_with("Error:"));
    }

    #[tokio::test]
    async fn command_status_reports_no_jobs_when_empty() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let output = registry
            .dispatch(&ToolCall {
                id: "c1".to_string(),
                name: "command_status".to_string(),
                arguments: "{}".to_string(),
            })
            .await;
        assert_eq!(output, "no background jobs");
    }

    #[tokio::test]
    async fn kill_all_jobs_stops_every_running_job() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let started = registry
            .dispatch(&ToolCall {
                id: "c1".to_string(),
                name: "run_command".to_string(),
                arguments: format!(r#"{{"command":"{}","background":true}}"#, sleep_command(30)),
            })
            .await;
        let job_id = job_id_from(&started);

        kill_all_jobs(&registry.jobs());

        let status = wait_for_job(&registry, &job_id).await;
        assert!(status.contains("status: killed"), "{status}");
    }

    fn patch_call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "c1".to_string(),
            name: "patch_file".to_string(),
            arguments: arguments.to_string(),
        }
    }

    #[tokio::test]
    async fn patch_file_replaces_an_exact_match() {
        let root = project_root();
        fs::write(root.join("main.rs"), "fn main() { old(); }\n").unwrap();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );

        let output = registry
            .dispatch(&patch_call(
                r#"{"path":"main.rs","old_text":"old();","new_text":"new();"}"#,
            ))
            .await;

        assert_eq!(output, "patched main.rs");
        assert_eq!(
            fs::read_to_string(root.join("main.rs")).unwrap(),
            "fn main() { new(); }\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_file_matches_across_line_endings() {
        let root = project_root();
        // A CRLF file, but the model's old_text uses LF.
        fs::write(
            root.join("crlf.txt"),
            "line one\r\nline two\r\nline three\r\n",
        )
        .unwrap();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );

        let output = registry
            .dispatch(&patch_call(
                r#"{"path":"crlf.txt","old_text":"line one\nline two","new_text":"line one\nLINE TWO"}"#,
            ))
            .await;

        assert_eq!(output, "patched crlf.txt");
        // The edit applies and the CRLF line endings are preserved.
        assert_eq!(
            fs::read_to_string(root.join("crlf.txt")).unwrap(),
            "line one\r\nLINE TWO\r\nline three\r\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_file_creates_a_new_file_when_old_text_is_empty() {
        let root = project_root();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );

        let output = registry
            .dispatch(&patch_call(
                r#"{"path":"NEW.md","old_text":"","new_text":"hello\n"}"#,
            ))
            .await;

        assert!(output.starts_with("created NEW.md"));
        assert_eq!(fs::read_to_string(root.join("NEW.md")).unwrap(), "hello\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_file_refuses_to_create_over_an_existing_file() {
        let root = project_root();
        fs::write(root.join("a.txt"), "content").unwrap();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );

        let output = registry
            .dispatch(&patch_call(
                r#"{"path":"a.txt","old_text":"","new_text":"x"}"#,
            ))
            .await;

        assert!(output.starts_with("Error:"));
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "content");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_file_rejects_missing_and_ambiguous_matches() {
        let root = project_root();
        fs::write(root.join("a.txt"), "one two one").unwrap();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );

        let missing = registry
            .dispatch(&patch_call(
                r#"{"path":"a.txt","old_text":"three","new_text":"x"}"#,
            ))
            .await;
        let ambiguous = registry
            .dispatch(&patch_call(
                r#"{"path":"a.txt","old_text":"one","new_text":"x"}"#,
            ))
            .await;

        assert!(missing.contains("not found"));
        assert!(ambiguous.contains("2 times"));
        // The file is untouched after both failures.
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "one two one"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_file_uses_a_unique_line_trimmed_fallback() {
        let root = project_root();
        fs::write(root.join("a.txt"), "fn main() {\n    let value = 1;\n}\n").unwrap();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );

        let output = registry
            .dispatch(&patch_call(
                r#"{"path":"a.txt","old_text":"fn main() {\n  let value = 1;\n}","new_text":"fn main() {\n    let value = 2;\n}"}"#,
            ))
            .await;
        assert!(!output.starts_with("Error:"), "{output}");
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "fn main() {\n    let value = 2;\n}\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_file_rejects_an_ambiguous_line_trimmed_fallback() {
        let root = project_root();
        fs::write(root.join("a.txt"), "  same\nother\n    same\n").unwrap();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );

        let output = registry
            .dispatch(&patch_call(
                r#"{"path":"a.txt","old_text":"\tsame","new_text":"changed"}"#,
            ))
            .await;
        assert!(output.contains("2 line-trimmed blocks"), "{output}");
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            "  same\nother\n    same\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn patch_file_rejects_paths_outside_the_project() {
        let root = project_root();
        let registry = ToolRegistry::with_defaults(
            root.clone(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );

        let output = registry
            .dispatch(&patch_call(
                r#"{"path":"../escape.txt","old_text":"","new_text":"x"}"#,
            ))
            .await;

        assert!(output.starts_with("Error:"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patch_target_resolves_within_the_project_root() {
        let root = project_root();
        let target =
            patch_target(&root, r#"{"path":"main.rs","old_text":"","new_text":"x"}"#).unwrap();

        assert_eq!(target, root.join("main.rs"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patch_target_returns_none_for_invalid_arguments() {
        let root = project_root();
        assert!(patch_target(&root, "not json").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patch_preview_shows_removed_and_added_lines() {
        let registry = ToolRegistry::with_defaults(
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
            CommandLimits::default(),
        );
        let preview = registry
            .preview(&patch_call(
                r#"{"path":"src/a.rs","old_text":"let a = 1;","new_text":"let a = 2;"}"#,
            ))
            .unwrap();

        assert!(preview.contains("--- src/a.rs"));
        assert!(preview.contains("- let a = 1;"));
        assert!(preview.contains("+ let a = 2;"));
    }

    /// Regression: `run_command` hands every command to `cmd /C` or `sh -c`, so the first word can
    /// be something the shell runs itself. Routing those used to rewrite a working `cd ..` into
    /// `rtk cd ..`, which fails with `rtk: Binary 'cd' not found on PATH`.
    #[test]
    fn never_routes_a_command_the_shell_runs_itself() {
        // Builtins of both shells, so these hold on every platform.
        assert!(!route_through_rtk("cd ..", true));
        assert!(!route_through_rtk("set", true));
        // An inline environment assignment is not a program name, and no builtin list can hold
        // every name a user might assign.
        assert!(!route_through_rtk("RUST_LOG=debug cargo test", true));
        // Which names are builtins follows the shell, not the platform: `cmd` runs `echo` and
        // `type` itself, while Unix has a real program for each.
        assert_eq!(route_through_rtk("echo hello", true), !cfg!(windows));
        assert_eq!(route_through_rtk("type notes.txt", true), !cfg!(windows));
        // `cmd` matches its builtins case-insensitively; `sh` does not, and Unix has no `DIR`
        // program either, so routing it there costs nothing.
        assert_eq!(route_through_rtk("DIR", true), !cfg!(windows));
        // A real program is still routed on both.
        assert!(route_through_rtk("cargo test", true));
    }

    #[test]
    fn routes_only_simple_commands_when_rtk_is_available() {
        assert!(route_through_rtk("cargo test", true));
        assert!(route_through_rtk("  git status  ", true));

        // Never without rtk.
        assert!(!route_through_rtk("cargo test", false));
        // Never double-prefix a command the model already routed.
        assert!(!route_through_rtk("rtk cargo test", true));
        assert!(!route_through_rtk("rtk", true));
        // Shell operators would leave rtk applied to the first segment only.
        assert!(!route_through_rtk("cargo build && cargo test", true));
        assert!(!route_through_rtk("cargo test | tail -5", true));
        assert!(!route_through_rtk("echo a; echo b", true));
        assert!(!route_through_rtk("cargo test > out.txt", true));
        assert!(!route_through_rtk("echo $HOME", true));
    }
}
