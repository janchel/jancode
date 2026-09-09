use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Lighter-clone of jancode-tool-types::ToolOutput + jancode-tool-core traits, kept
/// minimal: a plain string output plus an optional markdown rendering hint.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub output: String,
    pub title: Option<String>,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn new(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            title: None,
            is_error: false,
        }
    }

    pub fn error(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            title: None,
            is_error: true,
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
}

/// JSON Schema tool definition, matching the OpenAI function-calling shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// A tool call returned by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub input: Value,
}

/// A completed tool execution for appending to the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub output: String,
}

/// Trait mirroring jancode-tool-core::Tool, simplified for a single binary crate.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn to_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters(),
        }
    }
    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String>;
}

/// Context passed to tools, mirroring jancode's ToolContext (working dir, etc.).
pub struct ToolContext {
    pub working_dir: PathBuf,
}

impl ToolContext {
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.working_dir.join(p)
        }
    }
}

/// Lexically normalize a path (resolve `.` / `..`) without touching the
/// filesystem or following symlinks.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// True if `target` (after lexical normalization) is not inside `base`.
pub fn is_outside(base: &Path, target: &Path) -> bool {
    let base = normalize_path(base);
    let target = normalize_path(target);
    !target.starts_with(&base)
}

// ---------------------------------------------------------------------------
// Tool registry
// ---------------------------------------------------------------------------

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.push(Box::new(tool));
    }

    /// Register an already-boxed tool (e.g. from a dynamic source like MCP).
    pub fn register_boxed(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn all(&self) -> &[Box<dyn Tool>] {
        &self.tools
    }

    /// Look up a tool by name (after alias resolution).
    pub fn find(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.name() == name).map(|b| b.as_ref())
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The canonical set of tools that jancode exposes.
pub fn default_registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(BashTool);
    r.register(ReadTool);
    r.register(WriteTool);
    r.register(EditTool);
    r.register(ListDirTool);
    r.register(GlobTool);
    r.register(GrepTool);
    r.register(ApplyPatchTool);
    r.register(PlanTool);
    r
}

// ---------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its stdout/stderr. Use this for running\n\
         git, cargo, tests, file manipulation via CLI, or any quick inspection of the\n\
         environment. Avoid using this to edit files or do long-running foreground\n\
         tasks — prefer read/write/edit for edits and background processes via the\n\
         shell itself for long-running work."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute." },
                "timeout_ms": { "type": "integer", "default": 10000, "description": "Max execution time in milliseconds." }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .context("missing 'command' parameter")?;
        let timeout_ms = input
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(10000);

        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.working_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("failed to spawn shell")?;

        let output = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            async {
                let out = child.wait_with_output().await?;
                Ok::<_, std::io::Error>(out)
            },
        )
        .await
        .context("command timed out")?;

        let output = output?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut result = stdout.to_string();
        if !stderr.is_empty() {
            result.push_str("\n[stderr: ");
            result.push_str(&stderr);
            result.push(']');
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file from the filesystem. Returns the file contents. Use this to\n\
         inspect source code, configs, logs, or any text file you need to understand\n\
         before editing."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read." },
                "start_line": { "type": "integer", "description": "Optional 1-based line to start reading from." },
                "end_line": { "type": "integer", "description": "Optional 1-based line to stop reading at." }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .context("missing 'path' parameter")?;
        let full_path = ctx.resolve_path(path);
        let content = tokio::fs::read_to_string(&full_path)
            .await
            .with_context(|| format!("reading {}", full_path.display()))?;

        let start_line = input
            .get("start_line")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let end_line = input
            .get("end_line")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);

        let lines: Vec<&str> = content.lines().collect();
        let start = start_line.map(|n| n.saturating_sub(1)).unwrap_or(0);
        let end = end_line
            .map(|n| n.min(lines.len()))
            .unwrap_or(lines.len());

        let selected: Vec<&str> = lines[start..end].to_vec();
        Ok(selected.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write or overwrite a file with the given content. If the file exists,\n\
         its contents are replaced. Parent directories are created as needed."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write." },
                "content": { "type": "string", "description": "Full file content." }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .context("missing 'path' parameter")?;
        let content = input
            .get("content")
            .and_then(|v| v.as_str())
            .context("missing 'content' parameter")?;
        let full_path = ctx.resolve_path(path);

        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating parent dirs for {}", full_path.display()))?;
        }
        tokio::fs::write(&full_path, content)
            .await
            .with_context(|| format!("writing {}", full_path.display()))?;

        Ok(format!("wrote {} bytes to {}", content.len(), full_path.display()))
    }
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace an exact string in a file with a new string. Fails if the old\n\
         string is not found or not unique. Use 'read' first to see exact content."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file." },
                "old_string": { "type": "string", "description": "Exact text to replace." },
                "new_string": { "type": "string", "description": "Replacement text." }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .context("missing 'path' parameter")?;
        let old = input
            .get("old_string")
            .and_then(|v| v.as_str())
            .context("missing 'old_string' parameter")?;
        let new = input
            .get("new_string")
            .and_then(|v| v.as_str())
            .context("missing 'new_string' parameter")?;
        let full_path = ctx.resolve_path(path);

        let content = tokio::fs::read_to_string(&full_path)
            .await
            .with_context(|| format!("reading {}", full_path.display()))?;

        let count = content.matches(old).count();
        if count == 0 {
            anyhow::bail!("old_string not found in {}", full_path.display());
        }
        if count > 1 {
            anyhow::bail!(
                "old_string found {} times in {} (not unique)",
                count,
                full_path.display()
            );
        }

        let new_content = content.replace(old, new);
        tokio::fs::write(&full_path, &new_content)
            .await
            .with_context(|| format!("writing {}", full_path.display()))?;

        Ok(format!("replaced 1 occurrence in {}", full_path.display()))
    }
}

// ---------------------------------------------------------------------------
// list_dir
// ---------------------------------------------------------------------------

pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List the contents of a directory. Returns directories and files with\n\
         their types. This is the starting point for exploring a codebase."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path to list (default: current working dir)." }
            },
            "required": []
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let full_path = ctx.resolve_path(path);

        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(&full_path)
            .await
            .with_context(|| format!("listing {}", full_path.display()))?;

        while let Some(entry) = dir.next_entry().await.context("reading dir entry")? {
            let name = entry.file_name().to_string_lossy().to_string();
            let file_type = entry
                .file_type()
                .await
                .map(|ft| ft.is_dir())
                .unwrap_or(false);
            entries.push((name, file_type));
        }

        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut result = String::new();
        for (name, is_dir) in entries {
            if is_dir {
                result.push_str(&format!("{}/\n", name));
            } else {
                result.push_str(&format!("{}\n", name));
            }
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// glob
// ---------------------------------------------------------------------------

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern (e.g. `src/**/*.rs`). Returns matching\n\
         file paths relative to the current working directory."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern (e.g. `**/*.rs`)." },
                "path": { "type": "string", "description": "Base path to search from (default: current working dir)." }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .context("missing 'pattern' parameter")?;
        let base = input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let base_path = ctx.resolve_path(base);

        let matches = glob::glob(&base_path.join(pattern).to_string_lossy())
            .map_err(|e| anyhow::anyhow!("invalid glob pattern: {}", e))?;

        let mut results = Vec::new();
        for entry in matches {
            if let Ok(p) = entry {
                if let Ok(rel) = p.strip_prefix(&base_path) {
                    results.push(rel.to_string_lossy().to_string());
                }
            }
        }
        results.sort();
        Ok(results.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// grep (agentgrep-style)
// ---------------------------------------------------------------------------

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "agentgrep"
    }

    fn description(&self) -> &str {
        "Search file contents using a regex pattern. Returns matching lines with\n\
         file path and line number. Alias: 'grep'."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Regex pattern to search for." },
                "path": { "type": "string", "description": "Base path to search from (default: current working dir)." },
                "include": { "type": "string", "description": "Glob pattern to filter files (e.g. '*.rs')." }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .context("missing 'query' parameter")?;
        let base = input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let base_path = ctx.resolve_path(base);

        let re = regex::Regex::new(query)
            .with_context(|| format!("invalid regex: {}", query))?;

        let mut matches = Vec::new();

        // Walk all files recursively
        let mut stack = vec![base_path.clone()];
        while let Some(dir) = stack.pop() {
            if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        // Apply include filter
                    if let Some(inc) = input.get("include").and_then(|v| v.as_str()) {
                        let pat = glob::Pattern::new(inc).unwrap_or_else(|_| glob::Pattern::new("*").unwrap());
                        if !pat.matches_with(name, glob::MatchOptions::new()) {
                            continue;
                        }
                    }
                        // Read and search
                        if let Ok(content) = tokio::fs::read_to_string(&path).await {
                            for (lineno, line) in content.lines().enumerate() {
                                if re.is_match(line) {
                                    matches.push(format!(
                                        "{}:{}: {}",
                                        path.strip_prefix(&base_path)
                                            .unwrap_or(&path)
                                            .display(),
                                        lineno + 1,
                                        line.trim_end()
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(matches.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// apply_patch
// ---------------------------------------------------------------------------

/// Applies a unified diff (git-style) to one or more files in the working
/// directory with one call. This is the workhorse for multi-file changes:
/// create, modify, and delete files from a single `patch`.
pub struct ApplyPatchTool;

#[derive(Debug)]
enum DiffLine {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Debug)]
struct Hunk {
    old_start: usize,
    lines: Vec<DiffLine>,
}

#[derive(Debug)]
struct PatchFile {
    old_path: Option<String>,
    new_path: Option<String>,
    hunks: Vec<Hunk>,
}

fn patch_path(s: &str, prefix: &str) -> Option<String> {
    let s = s.trim();
    if s == "/dev/null" {
        None
    } else {
        Some(s.strip_prefix(prefix).unwrap_or(s).to_string())
    }
}

fn parse_patch(text: &str) -> Result<Vec<PatchFile>> {
    let mut files: Vec<PatchFile> = Vec::new();
    let empty = || PatchFile { old_path: None, new_path: None, hunks: Vec::new() };
    let mut cur = empty();
    let mut cur_hunk: Option<Hunk> = None;
    let mut in_body = false;

    let flush_hunk = |cur: &mut PatchFile, cur_hunk: &mut Option<Hunk>| {
        if let Some(h) = cur_hunk.take() {
            cur.hunks.push(h);
        }
    };
    let flush_file = |files: &mut Vec<PatchFile>, cur: &mut PatchFile, empty: &dyn Fn() -> PatchFile| {
        if cur.new_path.is_some() || !cur.hunks.is_empty() {
            files.push(std::mem::replace(cur, empty()));
        }
    };

    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            in_body = false;
            continue;
        }
        // Parse hunk body lines first (single-char prefix). Detect file/hunk
        // boundaries before consuming `---` / `+++` as removal/addition lines.
        if in_body {
            match line.as_bytes().first() {
                Some(b' ') => {
                    cur_hunk.as_mut().unwrap().lines.push(DiffLine::Context(line[1..].to_string()));
                    continue;
                }
                Some(b'-') if !line.starts_with("--- ") => {
                    cur_hunk.as_mut().unwrap().lines.push(DiffLine::Remove(line[1..].to_string()));
                    continue;
                }
                Some(b'+') if !line.starts_with("+++ ") => {
                    cur_hunk.as_mut().unwrap().lines.push(DiffLine::Add(line[1..].to_string()));
                    continue;
                }
                Some(b'\\') => continue, // "\ No newline at end of file"
                _ => { in_body = false; }
            }
        }
        if line.starts_with("@@ ") {
            flush_hunk(&mut cur, &mut cur_hunk);
            let num = line
                .trim_start_matches("@@ ")
                .split_once(' ')
                .map(|(a, _)| a)
                .unwrap_or_else(|| line.trim_start_matches("@@ "));
            let old_start: usize = num
                .trim_start_matches('-')
                .split(',')
                .next()
                .unwrap_or("1")
                .parse()
                .unwrap_or(1);
            cur_hunk = Some(Hunk { old_start, lines: Vec::new() });
            in_body = true;
            continue;
        }
        if line.starts_with("diff --git ") {
            flush_hunk(&mut cur, &mut cur_hunk);
            flush_file(&mut files, &mut cur, &empty);
            in_body = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            flush_hunk(&mut cur, &mut cur_hunk);
            if cur.new_path.is_some() {
                flush_file(&mut files, &mut cur, &empty);
            }
            cur.old_path = patch_path(rest, "a/");
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            flush_hunk(&mut cur, &mut cur_hunk);
            if cur.old_path.is_some() && cur.new_path.is_some() {
                flush_file(&mut files, &mut cur, &empty);
            }
            cur.new_path = patch_path(rest, "b/");
            continue;
        }
        // index / mode / similarity / any other metadata lines: ignore.
    }
    flush_hunk(&mut cur, &mut cur_hunk);
    flush_file(&mut files, &mut cur, &empty);
    Ok(files)
}

async fn apply_patch_file(file: &PatchFile, base_dir: &Path) -> Result<String> {
    let new_path = match &file.new_path {
        Some(p) => p,
        None => anyhow::bail!("patch has no target file (new_path missing)"),
    };

    if new_path == "/dev/null" {
        let old = file.old_path.as_deref().unwrap_or("");
        let full = base_dir.join(old);
        if tokio::fs::remove_file(&full).await.is_ok() {
            return Ok(format!("deleted {}", full.display()));
        }
        return Ok(format!("(delete skipped: {} not found)", full.display()));
    }

    let is_new = file.old_path.is_none();
    let full = base_dir.join(new_path);
    let content = if is_new {
        String::new()
    } else {
        match tokio::fs::read_to_string(&full).await {
            Ok(c) => c,
            Err(_) => {
                return Ok(format!(
                    "ERROR: cannot apply patch to {}: file does not exist",
                    full.display()
                ))
            }
        }
    };

    let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
    let mut out: Vec<String> = Vec::new();
    let mut idx = 0usize;
    let mut added = 0usize;
    let mut removed = 0usize;

    for hunk in &file.hunks {
        let target = hunk.old_start.saturating_sub(1);
        while idx < target && idx < lines.len() {
            out.push(lines[idx].clone());
            idx += 1;
        }
        for dline in &hunk.lines {
            match dline {
                DiffLine::Context(c) => {
                    if idx >= lines.len() || lines[idx] != *c {
                        return Ok(format!(
                            "ERROR: patch context mismatch in {} (expected {:?})",
                            full.display(),
                            c
                        ));
                    }
                    out.push(lines[idx].clone());
                    idx += 1;
                }
                DiffLine::Remove(c) => {
                    if idx >= lines.len() || lines[idx] != *c {
                        return Ok(format!(
                            "ERROR: patch removal mismatch in {} (expected {:?})",
                            full.display(),
                            c
                        ));
                    }
                    idx += 1;
                    removed += 1;
                }
                DiffLine::Add(c) => {
                    out.push(c.clone());
                    added += 1;
                }
            }
        }
    }
    while idx < lines.len() {
        out.push(lines[idx].clone());
        idx += 1;
    }

    let new_content = out.join("\n");
    if let Some(parent) = full.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    if let Err(e) = tokio::fs::write(&full, &new_content).await {
        return Ok(format!("ERROR: writing {}: {}", full.display(), e));
    }
    Ok(format!(
        "{} {} ({}+/{}- lines)",
        if is_new { "created" } else { "updated" },
        full.display(),
        added,
        removed
    ))
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Apply a unified diff to one or more files in the working directory with a\n\
         single call. Pass a git-style patch in the 'patch' parameter:\n\
         ```\n\
         --- a/existing.rs\n\
         +++ b/existing.rs\n\
         @@ -1,3 +1,4 @@\n\
          unchanged context line\n\
         -removed line\n\
         +added line\n\
         ```\n\
         Use it for coordinated multi-file edits: create new files (--- /dev/null),\n\
         modify several files, or delete files (+++ /dev/null). Fails loudly if the\n\
         context does not match the current file contents."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "The unified diff text (git-style, one or more files)."
                }
            },
            "required": ["patch"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let patch_text = input
            .get("patch")
            .and_then(|v| v.as_str())
            .context("missing 'patch' parameter")?;
        let files = parse_patch(patch_text)?;
        if files.is_empty() {
            anyhow::bail!("no valid file hunks found in patch");
        }
        let mut summary = format!("applied patch to {} file(s):", files.len());
        for f in &files {
            match apply_patch_file(f, &ctx.working_dir).await {
                Ok(s) => summary.push_str(&format!("\n  {}", s)),
                Err(e) => summary.push_str(&format!("\n  ERROR: {}", e)),
            }
        }
        Ok(summary)
    }
}

// ---------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------

/// Persists a step-by-step plan in `<working_dir>/.jancode-plan.md` so agents
/// can track multi-step work across turns and tool calls.
pub struct PlanTool;

#[async_trait]
impl Tool for PlanTool {
    fn name(&self) -> &str {
        "plan"
    }

    fn description(&self) -> &str {
        "Maintain a persistent step-by-step plan for this working directory in\n\
         .jancode-plan.md. Actions: 'create' (write 'steps'), 'append' (add more\n\
         steps), 'complete' (mark step N done by number), 'show' (print the plan).\n\
         Start a multi-step task by creating a plan, then update it as you go so\n\
         you never lose track or redo finished work."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "append", "complete", "show"],
                    "description": "What to do with the plan."
                },
                "steps": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Steps to write (create) or add (append)."
                },
                "step": {
                    "type": "integer",
                    "description": "1-based step number to mark complete."
                },
                "title": {
                    "type": "string",
                    "description": "Optional plan title set on create."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .context("missing 'action' parameter")?;
        let plan_file = ctx.working_dir.join(".jancode-plan.md");
        let steps_from = |v: Option<&Value>| -> Option<Vec<String>> {
            v.and_then(|v| v.as_array()).map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect()
            })
        };

        match action {
            "create" => {
                let steps = steps_from(input.get("steps")).context("missing 'steps' array for create")?;
                let title = input.get("title").and_then(|v| v.as_str()).unwrap_or("Plan");
                let mut out = format!("# {}\n\n", title);
                for s in &steps {
                    out.push_str(&format!("- [ ] {}\n", s));
                }
                tokio::fs::write(&plan_file, &out)
                    .await
                    .with_context(|| format!("writing {}", plan_file.display()))?;
                Ok(format!("plan updated: {} steps", steps.len()))
            }
            "append" => {
                let steps = steps_from(input.get("steps")).context("missing 'steps' array for append")?;
                let mut out = tokio::fs::read_to_string(&plan_file).await.unwrap_or_default();
                for s in &steps {
                    out.push_str(&format!("- [ ] {}\n", s));
                }
                tokio::fs::write(&plan_file, &out)
                    .await
                    .with_context(|| format!("writing {}", plan_file.display()))?;
                Ok(format!("added {} steps", steps.len()))
            }
            "complete" => {
                let n: usize = input
                    .get("step")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .context("missing 'step' number for complete")?;
                let data = tokio::fs::read_to_string(&plan_file)
                    .await
                    .with_context(|| format!("reading {}", plan_file.display()))?;
                let mut count = 0usize;
                let mut changed = false;
                let mut out = String::new();
                for l in data.lines() {
                    if let Some(rest) = l.strip_prefix("- [ ] ") {
                        count += 1;
                        if count == n {
                            out.push_str(&format!("- [x] {}\n", rest));
                            changed = true;
                        } else {
                            out.push_str(l);
                            out.push('\n');
                        }
                    } else {
                        out.push_str(l);
                        out.push('\n');
                    }
                }
                if !changed {
                    return Ok(format!("no step {} in plan ({} steps)", n, count));
                }
                tokio::fs::write(&plan_file, &out)
                    .await
                    .with_context(|| format!("writing {}", plan_file.display()))?;
                Ok(format!("completed step {}", n))
            }
            "show" => {
                let data = tokio::fs::read_to_string(&plan_file)
                    .await
                    .with_context(|| format!("no plan yet — create one with action=create"))?;
                Ok(data)
            }
            _ => Ok(format!("unknown plan action: {}", action)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_escape_detection() {
        let base = Path::new("/work/proj");
        assert!(!is_outside(base, &base.join("src/main.rs")));
        assert!(!is_outside(base, &base.join("src/../lib.rs")));
        assert!(is_outside(base, &base.join("../secrets.txt")));
        assert!(is_outside(base, &Path::new("/etc/passwd")));
        assert!(!is_outside(base, &Path::new("/work/proj")));
    }

    #[test]
    fn parses_multi_file_patch() {
        let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1,1 +1,2 @@
 hello
+world
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,1 @@
+content
";
        let files = parse_patch(patch).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path.as_deref(), Some("a.txt"));
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[1].old_path, None);
        assert_eq!(files[1].new_path.as_deref(), Some("new.txt"));
    }
}
