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
