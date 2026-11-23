use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// JSON Schema tool definition, matching the OpenAI function-calling shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
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

    /// Whether this tool is safe to run concurrently with other independent
    /// tools. Read-only tools that don't mutate shared state (files, the
    /// filesystem, external systems) return `true`; anything that writes,
    /// edits, or has side effects returns `false` (the default).
    fn is_independent(&self) -> bool {
        false
    }
}

/// Context passed to tools, mirroring jancode's ToolContext (working dir, etc.).
#[derive(Clone)]
pub struct ToolContext {
    pub working_dir: PathBuf,
    /// Optional `[database] url` from config, used by the `sql` tool when a
    /// query call omits its own `db`.
    pub database_url: String,
    /// `[server] bash_gate` setting: "off" | "basic" | "strict". Controls how
    /// aggressively the `bash` tool is gated for commands that reference paths
    /// outside the working directory.
    pub bash_gate: String,
    /// Paths the user explicitly approved for outside-workspace writes during
    /// this turn (populated by the approval gate). Shared through clones so the
    /// concurrent read-only tool fan-out sees the same set.
    pub allowed_outside: Arc<Mutex<HashSet<PathBuf>>>,
}

impl ToolContext {
    pub fn new(working_dir: PathBuf, database_url: String, bash_gate: String) -> Self {
        Self {
            working_dir,
            database_url,
            bash_gate,
            allowed_outside: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.working_dir.join(p)
        }
    }
}

/// The canonical built-in tool names, in registry order. Shared by the client
/// (tool allow-list) and the daemon (registry filtering) so the two can't drift.
pub const BUILTIN_TOOLS: &[&str] = &[
    "bash",
    "read",
    "write",
    "edit",
    "list_dir",
    "glob",
    "agentgrep",
    "apply_patch",
    "plan",
    "git",
    "fetch_url",
    "http_request",
    "note",
    "docker",
    "sql",
    "jq",
    "gh",
    "test",
];

/// Resolve a path for a *writing* tool and enforce workspace confinement.
///
/// `write`/`edit`/`apply_patch` accept arbitrary strings, so without this a
/// model (or a malicious prompt) could overwrite files outside the workspace
/// with `../../etc/...` or an absolute path. Paths are normalized lexically
/// (`.`/`..` resolved, symlinks not followed) and rejected when they escape
/// `ctx.working_dir` — unless the user explicitly approved that path for this
/// turn (see [`allow_outside`]). Headless agents never populate the allow-list,
/// so their writes are always confined.
pub fn resolve_within(ctx: &ToolContext, path: &str) -> Result<PathBuf> {
    let full = ctx.resolve_path(path);
    let normalized = normalize_path(&full);
    let base = normalize_path(&ctx.working_dir);
    if normalized.starts_with(&base) {
        return Ok(full);
    }
    let allowed = ctx
        .allowed_outside
        .lock()
        .map(|set| set.contains(&normalized))
        .unwrap_or(false);
    if allowed {
        return Ok(full);
    }
    anyhow::bail!(
        "path {} is outside the workspace {} (not approved)",
        full.display(),
        ctx.working_dir.display()
    );
}

/// Record an interactive approval for an outside-workspace write. Called by the
/// daemon's approval gate when the user answers `y`.
pub fn allow_outside(ctx: &ToolContext, path: &str) {
    let normalized = normalize_path(&ctx.resolve_path(path));
    if let Ok(mut set) = ctx.allowed_outside.lock() {
        set.insert(normalized);
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

/// Rewrite a leading `cd` in a bash command so the agent uses relative paths
/// (matching the CLI-agent convention) WITHOUT changing what it does.
///
/// - If the command starts with `cd <absolute-path>` where the target is INSIDE
///   the workspace, we rewrite the absolute path to its relative form (e.g.
///   `cd /home/user/proj/src && make` -> `cd src && make`). This removes the
///   absolute-path noise while keeping the exact same directory change.
/// - If the target is already relative, or the command has no leading `cd`,
///   nothing changes.
/// - If the target resolves OUTSIDE the workspace, we leave the command
///   completely untouched so the approval gate still sees the escape and can
///   prompt for approval. This preserves the core security function.
///
/// Returns the rewritten command when a leading absolute-path `cd` inside the
/// workspace was found, otherwise `None` (command unchanged).
pub fn normalize_bash_cd(command: &str, working_dir: &Path) -> Option<String> {
    let cmd = command.trim();
    if cmd.is_empty() {
        return None;
    }
    let first = cmd.split_whitespace().next().unwrap_or("");
    if first != "cd" {
        return None;
    }
    let rest = cmd.strip_prefix("cd").unwrap_or("").trim();
    if rest.is_empty() {
        return None; // `cd` with no arg = $HOME — leave for the gate.
    }
    let target = rest.split_whitespace().next().unwrap_or("");
    if target.starts_with("~") || target.starts_with("$HOME") {
        return None; // home — leave for the gate.
    }
    // Only rewrite an ABSOLUTE target that is inside the workspace down to a
    // relative path. Relative targets are already fine.
    if !target.starts_with("/") {
        return None;
    }
    let abs_target = std::path::PathBuf::from(target);
    if is_outside(working_dir, &abs_target) {
        return None; // outside the workspace — leave for the gate.
    }
    // Compute the relative form of the absolute target from the workspace.
    let rel = abs_target
        .strip_prefix(working_dir)
        .ok()?
        .to_string_lossy()
        .to_string();
    let rel = if rel.is_empty() { ".".to_string() } else { rel };
    // Rebuild: `cd <rel>` + everything after the target in the original
    // command, preserved verbatim (including `&&`, `;`, etc.).
    let remainder = rest[target.len()..].trim_start();
    if remainder.is_empty() {
        Some(format!("cd {}", rel))
    } else {
        Some(format!("cd {} {}", rel, remainder))
    }
}

/// Shell operators/separators — never file operands.
fn is_shell_operator(t: &str) -> bool {
    matches!(
        t,
        "&&" | "||" | ";" | "|" | ">" | ">>" | "2>" | "1>" | "<<" | "<<-" | "2>>" | "&>"
    )
}

/// Extract the likely target file/directory path(s) for a mutating command
/// (e.g. `sed -i`, `rm`, `mv file1 file2`, `touch a b`, `cat > out`). This is
/// shown in the approval reason so the user knows what the command would change.
/// It's best-effort: we skip the command name, flags (`-*`), sed/awk/perl
/// expression bodies, shell operators, and redirection operators, and collect
/// the remaining path-looking tokens.
fn mutation_targets(command: &str, mut_cmd: &str) -> Vec<String> {
    let tokens: Vec<&str> = command.split_whitespace().collect();

    // sed / awk / perl put their file operands AFTER the flags and expressions.
    // Expressions can contain spaces (`'s/rgba(1, 2/3, 4/'`), which splits them
    // across tokens, so scan backwards and take the trailing run of
    // path-looking tokens — the scan stops at the first token that isn't a path
    // (an expression fragment, a flag, or a shell operator). This also copes
    // with backslash line continuations, where `\` is itself a token, and with
    // several operands (`sed -i ... a.css b.css`).
    if matches!(mut_cmd, "sed" | "awk" | "perl") {
        let mut targets: Vec<String> = tokens
            .iter()
            .rev()
            .map_while(|t| {
                let t = t.trim_matches(['\'', '"']);
                (!t.starts_with('-') && !is_shell_operator(t) && is_likely_target(t))
                    .then(|| t.to_string())
            })
            .collect();
        targets.reverse();
        return targets;
    }

    // For `mv a b` / `cp a b`, the destination is the last path; for
    // `cat > out` / `>> out`, it's the token after the redirection; for
    // `touch a b`, both are targets. General path-like scan:
    let targets: Vec<&str> = tokens
        .iter()
        .enumerate()
        .filter_map(|(i, &t)| {
            if i == 0 && t == mut_cmd {
                return None;
            }
            if t.starts_with('-') {
                return None;
            }
            if is_shell_operator(t) || matches!(t, "if" | "then" | "else" | "fi") {
                return None;
            }
            let prev = if i > 0 { tokens[i - 1] } else { "" };
            if matches!(prev, ">" | ">>" | "2>" | "1>") {
                return Some(t.trim_matches(['\'', '"']));
            }
            if is_likely_target(t) {
                Some(t.trim_matches(['\'', '"']))
            } else {
                None
            }
        })
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for t in targets {
        if seen.insert(t.to_string()) {
            out.push(t.to_string());
        }
    }
    out
}

/// Best-effort: does this token look like a file path (something a mutating
/// command would target)? We look for a file extension or a path separator,
/// which distinguishes `styles.css` / `src/main.rs` from sed expressions and
/// shell operators.
fn is_likely_target(t: &str) -> bool {
    let t = t.trim_matches(['\'', '"']);
    if t.is_empty() {
        return false;
    }
    if t.starts_with("s/")
        || t.starts_with("y/")
        || t.starts_with("p;")
        || t.starts_with('/') && t.len() > 1
    {
        return false; // sed/awk expression or an operator-ish `/something`
    }
    if t.contains('/') {
        return true;
    }
    // A file extension (e.g. .css, .rs, .txt, .json) is a strong signal.
    Path::new(t).extension().is_some()
}

/// Does a sed/awk/perl invocation edit files in place? Accepts the separate
/// flag (`-i`), a value form (`-i.bak`), the long form (`--in-place[=SUFFIX]`),
/// and clustered short flags (`perl -pi`, `sed -ni`). The cluster case matters:
/// a naive `contains("-i")` misses `-pi`, silently letting an in-place rewrite
/// through ungated.
fn has_in_place_flag(command: &str) -> bool {
    command.split_whitespace().any(|t| {
        if t.starts_with("--") {
            return t.starts_with("--in-place");
        }
        // A cluster of short flags (`-i`, `-pi`, `-ni`, `-i.bak`): `i` is the
        // only lowercase 'i' a sed/awk/perl flag cluster uses for a file write.
        t.len() > 1 && t.starts_with('-') && t[1..].contains('i')
    })
}

/// The mutating command in a `bash` command string (`sed -i`, `rm`, `tee`,
/// `cat > file`, ...), or `None` when the command doesn't write files.
/// `"shell-redirect"` stands in for a bare write redirection.
///
/// Single source of truth for "could this `bash` call change a file?": the
/// approval gate uses it to decide whether to ask, and the client uses it (via
/// `bash_mutation_targets`) to decide whether to look up a diff afterwards.
pub fn bash_mutation(command: &str) -> Option<&str> {
    // sed/awk/perl only mutate with -i / --in-place; a plain
    // `sed s/a/b/ file` just prints to stdout.
    let in_place = has_in_place_flag(command);
    let mutating = command.split_whitespace().find(|t| match *t {
        "sed" | "awk" | "perl" => in_place,
        "rm" | "rmdir" | "mv" | "cp" | "install" | "truncate" | "unlink" | "dd" | "nano"
        | "vim" | "vi" | "ed" | "tee" | "touch" | "mkdir" | "chmod" | "chown" | "chattr" => true,
        _ => false,
    });
    if mutating.is_some() {
        return mutating;
    }
    // Also flag shell redirection writing to a file (`>`, `>>`, `2>`, `1>`) —
    // but a lone `>` could be a comparison, so we only flag a standalone
    // operator token.
    command
        .split_whitespace()
        .any(|t| matches!(t, ">" | ">>" | "2>" | "1>"))
        .then_some("shell-redirect")
}

/// Best-effort paths a mutating `bash` command writes to, or `None` when the
/// command is read-only (so callers can skip the work entirely). Used to scope
/// a post-hoc `git diff` to just the files the command touched.
pub fn bash_mutation_targets(command: &str) -> Option<Vec<String>> {
    let mut_cmd = bash_mutation(command)?;
    Some(mutation_targets(command, mut_cmd))
}

/// Heuristic: does a `bash` command reference files/directories outside the
/// working directory? `bash` is a free-form shell string, so we can't parse it
/// reliably — this is a conservative best-effort scan that flags obvious
/// escapes (absolute paths, `..`, `~`, `$HOME`, `$PWD`-relative tricks) while
/// letting ordinary in-workspace commands run ungated.
///
/// `mode` mirrors the `[server] bash_gate` config:
/// - "off":    never gate (returns `None` always).
/// - "basic":  gate on obvious escapes (absolute paths, `~`, `$HOME`, `..`,
///   leading `cd` out of the workspace).
/// - "strict": also gate on any `cd`, `$PWD`/`$OLDPWD` tricks, and commands
///   that read env vars pointing outside.
///
/// Returns `Some(reason)` when the command looks like it touches something
/// outside the workspace, `None` when it appears safe.
pub fn bash_escapes_workspace(command: &str, working_dir: &Path, mode: &str) -> Option<String> {
    if mode == "off" {
        return None;
    }
    let cmd = command.trim();

    // Empty / trivial commands are safe.
    if cmd.is_empty() {
        return None;
    }

    // A leading `cd` that leaves the workspace is a strong signal.
    // e.g. `cd ~/sysadmin-mcp && grep ...`, `cd /etc && cat hosts`.
    // But a `cd` that targets a path INSIDE the workspace is harmless (the
    // agent is just anchoring to the project root/subdir).
    let first = cmd.split_whitespace().next().unwrap_or("");
    if first == "cd" {
        // `cd` with no arg goes to $HOME — outside the workspace.
        let rest = cmd.strip_prefix("cd").unwrap_or("").trim();
        if rest.is_empty() {
            return Some("cd to $HOME (outside workspace)".to_string());
        }
        let target = rest.split_whitespace().next().unwrap_or("");
        // `~` / `$HOME` always point at the home dir, outside the workspace.
        if target.starts_with("~") || target.starts_with("$HOME") {
            return Some(format!("cd to {} (outside workspace)", target));
        }
        // Resolve an absolute/relative target against the workspace and check
        // whether it lands inside it. If so, the cd is safe.
        let abs_target = if target.starts_with("/") {
            std::path::PathBuf::from(target)
        } else if let Some(rel) = target.strip_prefix("$PWD/") {
            working_dir.join(rel)
        } else {
            working_dir.join(target)
        };
        // In strict mode, ANY cd is treated as potentially escaping (the cd
        // could be used to reach anything even if it starts in-workspace).
        if mode == "strict" {
            return Some(format!("cd to {} (strict mode)", target));
        }
        if is_outside(working_dir, &abs_target) {
            if target == ".." || target.starts_with("../") || target.starts_with("/") {
                return Some(format!("cd to {} (outside workspace)", target));
            }
            if mode == "strict" {
                return Some(format!("cd to {} (strict mode)", target));
            }
        }
        // Inside-workspace cd (absolute to the project, or a bare subdir
        // resolved within it): fall through ungated.
    }

    // File-mutation commands modify in-workspace files, so they should require
    // approval even though they don't "escape" the workspace. This prevents
    // `bash sed -i 's/x/y/' styles.css` from silently rewriting source files
    // (which is exactly what write/edit/apply_patch gate). We scan EVERY token
    // (not just the first) so `cd ... && sed -i ...` is still caught.
    if let Some(c) = bash_mutation(cmd) {
        // Gate in-workspace file mutation. Out-of-workspace mutations are
        // separately flagged by the path-escape token scan below.
        let targets = mutation_targets(cmd, c);
        let targets_str = if targets.is_empty() {
            String::new()
        } else {
            format!(" on {}", targets.join(", "))
        };
        return Some(format!(
            "modifies file(s){} via {} (in-workspace writes require approval)",
            targets_str, c
        ));
    }

    // Scan every whitespace-delimited token for path escapes. We skip heredoc
    // bodies (`<< 'EOF' ... EOF`) because their content is data (e.g. CSS/JS
    // with `/*` comments), not paths.
    let mut in_heredoc = false;
    let mut heredoc_delim: Option<String> = None;
    for tok in cmd.split_whitespace() {
        // Detect heredoc start. Two forms:
        //   `<< 'EOF'`  — `<<` and delimiter are separate tokens
        //   `<<EOF`     — delimiter is glued to `<<`
        if !in_heredoc {
            if tok == "<<" || tok == "<<-" {
                in_heredoc = true;
                continue;
            }
            if tok.starts_with("<<") && tok.len() > 2 {
                in_heredoc = true;
                heredoc_delim = Some(tok[2..].trim_matches(['\'', '"']).to_string());
                continue;
            }
        }
        if in_heredoc {
            // If we haven't captured the delimiter yet, this token is it.
            if heredoc_delim.is_none() {
                heredoc_delim = Some(tok.trim_matches(['\'', '"']).to_string());
                continue;
            }
            // If we see the delimiter again, the heredoc body is over.
            if let Some(d) = &heredoc_delim {
                if tok == d {
                    in_heredoc = false;
                    heredoc_delim = None;
                    continue;
                }
            }
            // Inside the heredoc body: content is data, not a path.
            continue;
        }

        // Skip shell operators, flags, and command names.
        if tok.is_empty() || tok.starts_with("-") || tok.starts_with("$(") || tok.starts_with("`") {
            continue;
        }
        // Absolute paths escape the workspace UNLESS they point inside it
        // (e.g. a command that names the project dir explicitly). A bare `/`
        // (e.g. `ls /` or a lone slash) is not a meaningful escape, and
        // `/*`/`*/` are comment markers, not paths.
        if tok.starts_with("/") && tok != "/" && !tok.starts_with("/*") && !tok.starts_with("*/") {
            let trimmed = tok.trim_end_matches('/');
            if is_outside(working_dir, std::path::Path::new(trimmed)) {
                return Some(format!("references absolute path {}", tok));
            }
            // otherwise: absolute path inside the workspace — safe.
        }
        // `~` / `$HOME` expand to the user's home, outside the workspace.
        if tok.starts_with("~") || tok.starts_with("$HOME") {
            return Some(format!("references {}", tok));
        }
        // `..` / `../...` walk up from the workspace.
        if tok == ".." || tok.starts_with("../") {
            return Some(format!("references {}", tok));
        }
        // `$PWD/..` or `$OLDPWD`-style escapes.
        if tok.starts_with("$PWD/..") || tok.starts_with("$OLDPWD") {
            return Some(format!("references {}", tok));
        }
        // In strict mode, any `$PWD`/`$OLDPWD` reference is treated as
        // potentially escaping (the cwd could be anywhere).
        if mode == "strict" && (tok.starts_with("$PWD") || tok.starts_with("$OLDPWD")) {
            return Some(format!("references {} (strict mode)", tok));
        }
    }

    None
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

    /// Keep only tools whose names appear in `names`. Used to honor the
    /// client's `tools` allow-list instead of always sending the full registry.
    pub fn retain_names(&mut self, names: &[String]) {
        self.tools.retain(|t| names.iter().any(|n| n == t.name()));
    }

    pub fn all(&self) -> &[Box<dyn Tool>] {
        &self.tools
    }

    /// Look up a tool by name (after alias resolution).
    pub fn find(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|b| b.as_ref())
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
    r.register(GitTool);
    r.register(WebFetchTool);
    r.register(HttpRequestTool);
    r.register(NoteTool);
    r.register(DockerTool);
    r.register(SqlTool);
    r.register(JqTool);
    r.register(GhTool);
    r.register(TestTool);
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

        // Normalize a leading `cd <inside-workspace>` prefix to a relative,
        // workspace-rooted invocation. This matches the CLI-agent convention
        // (agents use relative paths from the launch folder). Commands that cd
        // OUTSIDE the workspace are left untouched so the approval gate still
        // sees the escape; by the time this runs, such commands have already
        // been approved, but the rewrite must not mask an external cd.
        let command =
            normalize_bash_cd(command, &ctx.working_dir).unwrap_or_else(|| command.to_string());

        let child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&command)
            .current_dir(&ctx.working_dir)
            // Kill the child if the timeout (or a `Cancel`) drops the future;
            // otherwise an orphaned command keeps running after the tool call.
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("failed to spawn shell")?;

        let output = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), async {
            let out = child.wait_with_output().await?;
            Ok::<_, std::io::Error>(out)
        })
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

    fn is_independent(&self) -> bool {
        true
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
        let end = end_line.map(|n| n.min(lines.len())).unwrap_or(lines.len());
        // Clamp: an inverted range (`start_line > end_line`) used to panic on
        // `lines[start..end]`, killing the turn task without a `Done`.
        let start = start.min(end);

        let selected: Vec<&str> = if start >= end {
            Vec::new()
        } else {
            lines[start..end].to_vec()
        };
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
        let full_path = resolve_within(ctx, path)?;

        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating parent dirs for {}", full_path.display()))?;
        }
        tokio::fs::write(&full_path, content)
            .await
            .with_context(|| format!("writing {}", full_path.display()))?;

        Ok(format!(
            "wrote {} bytes to {}",
            content.len(),
            full_path.display()
        ))
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
        let full_path = resolve_within(ctx, path)?;

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

    fn is_independent(&self) -> bool {
        true
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
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
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

    fn is_independent(&self) -> bool {
        true
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
        let base = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let base_path = ctx.resolve_path(base);

        let matches = glob::glob(&base_path.join(pattern).to_string_lossy())
            .map_err(|e| anyhow::anyhow!("invalid glob pattern: {}", e))?;

        let mut results = Vec::new();
        for p in matches.flatten() {
            if let Ok(rel) = p.strip_prefix(&base_path) {
                results.push(rel.to_string_lossy().to_string());
            }
        }
        results.sort();
        Ok(results.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// grep (agentgrep-style)
// ---------------------------------------------------------------------------

/// Directories skipped during a recursive grep (VCS metadata, dependencies,
/// build output) so a broad search doesn't churn through them.
const GREP_IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
];

/// Files larger than this are skipped by `agentgrep` (binary/generated files).
const GREP_MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Cap on reported matches so one broad regex can't flood the model context.
const GREP_MAX_MATCHES: usize = 5000;

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

    fn is_independent(&self) -> bool {
        true
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
        let base = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let base_path = ctx.resolve_path(base);

        let re = regex::Regex::new(query).with_context(|| format!("invalid regex: {}", query))?;

        let mut matches = Vec::new();

        // Walk all files recursively. Symlinks are skipped (they can cycle or
        // escape the base), as are VCS/dependency/build directories and files
        // too large to be sensible search targets.
        let mut stack = vec![base_path.clone()];
        'walk: while let Some(dir) = stack.pop() {
            let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let Ok(file_type) = entry.file_type().await else {
                    continue;
                };
                let path = entry.path();
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if GREP_IGNORED_DIRS.contains(&name.as_ref()) {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                // Apply include filter
                if let Some(inc) = input.get("include").and_then(|v| v.as_str()) {
                    let pat = glob::Pattern::new(inc)
                        .unwrap_or_else(|_| glob::Pattern::new("*").unwrap());
                    if !pat.matches_with(name, glob::MatchOptions::new()) {
                        continue;
                    }
                }
                if let Ok(md) = entry.metadata().await {
                    if md.len() > GREP_MAX_FILE_BYTES {
                        continue;
                    }
                }
                // Read and search
                let Ok(content) = tokio::fs::read_to_string(&path).await else {
                    continue;
                };
                for (lineno, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        matches.push(format!(
                            "{}:{}: {}",
                            path.strip_prefix(&base_path).unwrap_or(&path).display(),
                            lineno + 1,
                            line.trim_end()
                        ));
                        if matches.len() >= GREP_MAX_MATCHES {
                            matches
                                .push(format!("... (truncated at {} matches)", GREP_MAX_MATCHES));
                            break 'walk;
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

/// Paths a unified diff touches, used for approval prompts and diff snapshots.
/// `+++` targets are used; deletions (`/dev/null`) are skipped. Best-effort and
/// shared by the daemon (gate reason) and the client (status line/snapshots).
pub fn patch_paths(patch: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let p = rest.trim();
            if p == "/dev/null" {
                continue;
            }
            let p = p.strip_prefix("b/").unwrap_or(p);
            if !paths.iter().any(|existing| existing == p) {
                paths.push(p.to_string());
            }
        }
    }
    paths
}

fn parse_patch(text: &str) -> Result<Vec<PatchFile>> {
    let mut files: Vec<PatchFile> = Vec::new();
    let empty = || PatchFile {
        old_path: None,
        new_path: None,
        hunks: Vec::new(),
    };
    let mut cur = empty();
    let mut cur_hunk: Option<Hunk> = None;
    let mut in_body = false;

    let flush_hunk = |cur: &mut PatchFile, cur_hunk: &mut Option<Hunk>| {
        if let Some(h) = cur_hunk.take() {
            cur.hunks.push(h);
        }
    };
    let flush_file =
        |files: &mut Vec<PatchFile>, cur: &mut PatchFile, empty: &dyn Fn() -> PatchFile| {
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
                    cur_hunk
                        .as_mut()
                        .unwrap()
                        .lines
                        .push(DiffLine::Context(line[1..].to_string()));
                    continue;
                }
                Some(b'-') if !line.starts_with("--- ") => {
                    cur_hunk
                        .as_mut()
                        .unwrap()
                        .lines
                        .push(DiffLine::Remove(line[1..].to_string()));
                    continue;
                }
                Some(b'+') if !line.starts_with("+++ ") => {
                    cur_hunk
                        .as_mut()
                        .unwrap()
                        .lines
                        .push(DiffLine::Add(line[1..].to_string()));
                    continue;
                }
                Some(b'\\') => continue, // "\ No newline at end of file"
                _ => {
                    in_body = false;
                }
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
            cur_hunk = Some(Hunk {
                old_start,
                lines: Vec::new(),
            });
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

async fn apply_patch_file(file: &PatchFile, ctx: &ToolContext) -> Result<String> {
    let new_path = match &file.new_path {
        Some(p) => p,
        None => anyhow::bail!("patch has no target file (new_path missing)"),
    };

    if new_path == "/dev/null" {
        let old = file.old_path.as_deref().unwrap_or("");
        let full = resolve_within(ctx, old)?;
        if tokio::fs::remove_file(&full).await.is_ok() {
            return Ok(format!("deleted {}", full.display()));
        }
        return Ok(format!("(delete skipped: {} not found)", full.display()));
    }

    let is_new = file.old_path.is_none();
    let full = resolve_within(ctx, new_path)?;
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
            match apply_patch_file(f, ctx).await {
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
                let steps =
                    steps_from(input.get("steps")).context("missing 'steps' array for create")?;
                let title = input
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Plan");
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
                let steps =
                    steps_from(input.get("steps")).context("missing 'steps' array for append")?;
                let mut out = tokio::fs::read_to_string(&plan_file)
                    .await
                    .unwrap_or_default();
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
                    .with_context(|| "no plan yet — create one with action=create".to_string())?;
                Ok(data)
            }
            _ => Ok(format!("unknown plan action: {}", action)),
        }
    }
}

// ---------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------

/// A single git tool with structured actions, mirroring what other CLI agents
/// expose: status, branch, checkout, diff, log, add, commit, push, pull,
/// remote, and stash. Read-only actions are never approval-gated; mutating
/// actions (add/commit/push/pull/checkout/branch -d/stash mutations) are gated
/// by `gate_tool` so interactive sessions confirm them.
pub struct GitTool;

const GIT_ACTIONS: &str =
    "status, branch, checkout, diff, log, add, commit, push, pull, fetch, merge, rebase, reset, remote, stash";

#[async_trait]
impl Tool for GitTool {
    fn name(&self) -> &str {
        "git"
    }

    fn description(&self) -> &str {
        "Manage a git repository in the working directory. Actions:\n\
         - status: show branch, staged/unstaged/untracked changes\n\
         - branch: list branches (pass branch to create/delete)\n\
         - checkout: switch branch (create_branch=true to make + switch)\n\
         - diff: show changes (staged=true for --cached, path to limit)\n\
         - log: recent commits (max = count, default 10)\n\
         - add: stage files (path = file/pattern, or \"all\" for .)\n\
         - commit: commit staged changes with message\n\
         - push: push commits (remote, refspec; force=true to overwrite)\n\
         - pull: pull from remote (refspec optional)\n\
         - fetch: update remote-tracking refs without merging (refspec optional)\n\
         - merge: merge <branch> into the current branch (allow_unrelated to use\n\
           --allow-unrelated-histories; abort=true to cancel a conflicted merge)\n\
         - rebase: replay current branch onto <branch> (abort/continue to resolve\n\
           conflicts step by step)\n\
         - reset: move HEAD to <ref> (default HEAD) with mode soft|mixed|hard\n\
         - remote: list remotes\n\
         - stash: list/save/pop/drop via stash_action\n\
         Use status/diff/log before committing, commit with a clear message,\n\
         then push. Mutations (add/commit/push/pull/merge/rebase/reset) ask for\n\
         approval in the interactive chat."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "branch", "checkout", "diff", "log", "add", "commit", "push", "pull", "fetch", "merge", "rebase", "reset", "remote", "stash"],
                    "description": "git subcommand to run."
                },
                "path": { "type": "string", "description": "File/pattern to add or limit diff to; 'all' stages everything." },
                "branch": { "type": "string", "description": "Branch/ref to switch to, merge/rebase from, or report on." },
                "create_branch": { "type": "boolean", "description": "With checkout: create then switch." },
                "delete_branch": { "type": "boolean", "description": "With branch: delete the branch." },
                "message": { "type": "string", "description": "Commit message (commit) or stash message (stash save)." },
                "staged": { "type": "boolean", "description": "With diff: show only staged changes (--cached)." },
                "max": { "type": "integer", "description": "With log: number of commits (default 10)." },
                "remote": { "type": "string", "description": "Remote name (default origin)." },
                "refspec": { "type": "string", "description": "Refspec/remote branch for push/pull/fetch (e.g. main or main:main)." },
                "force": { "type": "boolean", "description": "With push: force overwrite (use with care)." },
                "mode": { "type": "string", "enum": ["soft", "mixed", "hard"], "description": "With reset: how far to move HEAD and the index/working tree." },
                "ref": { "type": "string", "description": "With reset: where to move HEAD (default HEAD, e.g. HEAD~1, a commit hash, or origin/main)." },
                "abort": { "type": "boolean", "description": "With merge/rebase: cancel the operation and return to the pre-operation state." },
                "rebase_continue": { "type": "boolean", "description": "With rebase: continue after resolving a conflict (stage fixes, then run)." },
                "allow_unrelated": { "type": "boolean", "description": "With merge: allow merging unrelated histories (--allow-unrelated-histories)." },
                "stash_action": { "type": "string", "enum": ["list", "save", "pop", "drop"], "description": "Which stash operation to run when action=stash." }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .context("missing 'action' parameter")?;

        let git = |args: Vec<&str>| {
            let args: Vec<String> = args.into_iter().map(|s| s.to_string()).collect();
            Self::run_git(ctx, args)
        };

        match action {
            "status" => git(vec!["status", "--short", "--branch"]).await,
            "branch" => {
                let branch = input.get("branch").and_then(|v| v.as_str()).unwrap_or("");
                if branch.is_empty() {
                    git(vec!["branch", "-avv"]).await
                } else if input
                    .get("delete_branch")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    git(vec!["branch", "-D", branch]).await
                } else {
                    git(vec!["branch", branch]).await
                }
            }
            "checkout" => {
                let branch = input
                    .get("branch")
                    .and_then(|v| v.as_str())
                    .context("missing 'branch' for checkout")?;
                if input
                    .get("create_branch")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    git(vec!["checkout", "-b", branch]).await
                } else {
                    git(vec!["checkout", branch]).await
                }
            }
            "diff" => {
                let mut args = vec!["diff"];
                if input
                    .get("staged")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    args.push("--cached");
                }
                if let Some(p) = input.get("path").and_then(|v| v.as_str()) {
                    if !p.is_empty() {
                        args.push("--");
                        args.push(p);
                    }
                }
                git(args).await
            }
            "log" => {
                let max = input.get("max").and_then(|v| v.as_u64()).unwrap_or(10);
                git(vec![
                    "log",
                    "--oneline",
                    "--decorate",
                    "-n",
                    &max.to_string(),
                ])
                .await
            }
            "add" => {
                let p = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let target = if p == "all" { "." } else { p };
                git(vec!["add", "--", target]).await
            }
            "commit" => {
                let message = input
                    .get("message")
                    .and_then(|v| v.as_str())
                    .context("missing 'message' for commit")?;
                git(vec!["commit", "-m", message]).await
            }
            "push" => {
                let remote = input
                    .get("remote")
                    .and_then(|v| v.as_str())
                    .unwrap_or("origin");
                let mut args = vec!["push"];
                if input
                    .get("force")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    args.push("--force");
                }
                args.push(remote);
                if let Some(rs) = input.get("refspec").and_then(|v| v.as_str()) {
                    if !rs.is_empty() {
                        args.push(rs);
                    }
                }
                git(args).await
            }
            "pull" => {
                let remote = input
                    .get("remote")
                    .and_then(|v| v.as_str())
                    .unwrap_or("origin");
                let mut args = vec!["pull", remote];
                if let Some(rs) = input.get("refspec").and_then(|v| v.as_str()) {
                    if !rs.is_empty() {
                        args.push(rs);
                    }
                }
                git(args).await
            }
            "fetch" => {
                let remote = input
                    .get("remote")
                    .and_then(|v| v.as_str())
                    .unwrap_or("origin");
                let mut args = vec!["fetch", remote];
                if let Some(rs) = input.get("refspec").and_then(|v| v.as_str()) {
                    if !rs.is_empty() {
                        args.push(rs);
                    }
                }
                git(args).await
            }
            "merge" => {
                if input
                    .get("abort")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    return git(vec!["merge", "--abort"]).await;
                }
                let branch = input
                    .get("branch")
                    .and_then(|v| v.as_str())
                    .context("missing 'branch' to merge")?;
                let mut args = vec!["merge"];
                if input
                    .get("allow_unrelated")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    args.push("--allow-unrelated-histories");
                }
                args.push(branch);
                git(args).await
            }
            "rebase" => {
                if input
                    .get("abort")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    return git(vec!["rebase", "--abort"]).await;
                }
                if input
                    .get("rebase_continue")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    return git(vec!["rebase", "--continue"]).await;
                }
                let branch = input
                    .get("branch")
                    .and_then(|v| v.as_str())
                    .context("missing 'branch' to rebase onto")?;
                git(vec!["rebase", branch]).await
            }
            "reset" => {
                let mode = input
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("mixed");
                if !matches!(mode, "soft" | "mixed" | "hard") {
                    return Ok(format!(
                        "reset mode must be soft, mixed, or hard (got: {})",
                        mode
                    ));
                }
                let flag = format!("--{}", mode);
                let mut args = vec!["reset", &flag];
                let r = input.get("ref").and_then(|v| v.as_str()).unwrap_or("HEAD");
                if !r.is_empty() {
                    args.push(r);
                }
                git(args).await
            }
            "remote" => git(vec!["remote", "-v"]).await,
            "stash" => {
                let sa = input
                    .get("stash_action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("list");
                match sa {
                    "save" => {
                        let msg = input
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("wip");
                        git(vec!["stash", "save", msg]).await
                    }
                    "pop" => git(vec!["stash", "pop"]).await,
                    "drop" => git(vec!["stash", "drop"]).await,
                    _ => git(vec!["stash", "list"]).await,
                }
            }
            _ => Ok(format!(
                "unknown git action: {} (available: {})",
                action, GIT_ACTIONS
            )),
        }
    }
}

impl GitTool {
    async fn run_git(ctx: &ToolContext, args: Vec<String>) -> Result<String> {
        // Never open an interactive editor: rebase --continue / merge reuse the
        // original or passed message. "true" is a no-op editor that exits 0.
        let env = [("GIT_EDITOR", "true"), ("GIT_MERGE_AUTOEDIT", "no")];
        run_cli("git", ctx, args, 60, &env).await
    }
}

/// Shared runner for external CLI tools (git/docker/psql/sqlite3/mysql),
/// mirroring the git runner: cwd = working dir, piped stdout/stderr, timeout,
/// and non-zero exits surfaced as formatted tool output rather than a hard
/// error so the model can react to failure text.
async fn run_cli(
    exe: &str,
    ctx: &ToolContext,
    args: Vec<String>,
    timeout_secs: u64,
    env: &[(&str, &str)],
) -> Result<String> {
    run_cli_inner(exe, ctx, args, None, timeout_secs, env).await
}

/// Like [`run_cli`], but feeds `stdin_data` to the child's stdin (closed after
/// writing so the child sees EOF).
async fn run_cli_with_stdin(
    exe: &str,
    ctx: &ToolContext,
    args: Vec<String>,
    stdin_data: &str,
    timeout_secs: u64,
    env: &[(&str, &str)],
) -> Result<String> {
    run_cli_inner(exe, ctx, args, Some(stdin_data), timeout_secs, env).await
}

async fn run_cli_inner(
    exe: &str,
    ctx: &ToolContext,
    args: Vec<String>,
    stdin_data: Option<&str>,
    timeout_secs: u64,
    env: &[(&str, &str)],
) -> Result<String> {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(&args)
        .current_dir(&ctx.working_dir)
        // Don't leave an orphaned git/docker/psql process behind on timeout.
        .kill_on_drop(true)
        .stdin(if stdin_data.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {} (is it installed?)", exe))?;

    if let Some(data) = stdin_data {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(data.as_bytes()).await;
        }
    }

    let output = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
        let out = child.wait_with_output().await?;
        Ok::<_, std::io::Error>(out)
    })
    .await
    .map_err(|_| anyhow::anyhow!("{} timed out after {}s", exe, timeout_secs))??;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        let msg = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        return Ok(format!(
            "{} failed (exit {}): {}",
            exe,
            output.status.code().unwrap_or(-1),
            msg
        ));
    }
    let mut out = stdout.trim().to_string();
    if !stderr.trim().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(stderr.trim());
    }
    if out.is_empty() {
        Ok(format!("{}: ok (no output)", exe))
    } else {
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// fetch_url
// ---------------------------------------------------------------------------

/// True for loopback / private / link-local / CGNAT / documentation addresses.
/// Used to stop the HTTP tools from being an ungated path to cloud metadata
/// (`169.254.169.254`) or internal services.
fn is_non_global_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.octets()[0] == 0
                // 100.64.0.0/10 (CGNAT)
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40)
        }
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
        }
    }
}

/// Reject URLs whose host is (or resolves to) a non-global address.
async fn assert_public_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).context("invalid URL")?;
    let host = parsed.host_str().context("URL has no host")?.to_string();
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if is_non_global_ip(&ip) {
            anyhow::bail!("refusing to fetch non-global address {}", ip);
        }
        return Ok(());
    }
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .context("resolving URL host")?
        .collect();
    if addrs.is_empty() {
        anyhow::bail!("URL host did not resolve: {}", host);
    }
    for addr in addrs {
        if is_non_global_ip(&addr.ip()) {
            anyhow::bail!("refusing to fetch non-global address {}", addr.ip());
        }
    }
    Ok(())
}

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "fetch_url"
    }

    fn description(&self) -> &str {
        "Fetch a URL (http/https) and return its text content. Use to read\n\
         documentation, inspect a web page, or pull data from an API endpoint.\n\
         Read-only and ungated. URLs that resolve to loopback, private, or\n\
         link-local addresses (e.g. cloud metadata) are refused."
    }

    fn is_independent(&self) -> bool {
        true
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The URL to fetch." },
                "max_chars": { "type": "integer", "default": 12000, "description": "Truncate the response to this many characters." }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: &Value, _ctx: &ToolContext) -> Result<String> {
        let url = input
            .get("url")
            .and_then(|v| v.as_str())
            .context("missing 'url'")?;
        assert_public_url(url).await?;
        let max_chars = input
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .unwrap_or(12000) as usize;
        let client = reqwest::Client::builder()
            .user_agent("jancode-agent/0.1")
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let resp = client.get(url).send().await.context("fetching URL")?;
        if !resp.status().is_success() {
            let status_code = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Ok(format!(
                "HTTP {}: {}",
                status_code,
                body.chars().take(500).collect::<String>()
            ));
        }
        let text = resp.text().await.context("reading response body")?;
        let truncated: String = text.chars().take(max_chars).collect();
        if truncated.len() < text.len() {
            Ok(format!(
                "{}\n\n[truncated: {} chars total, set max_chars higher to see more]",
                truncated,
                text.len()
            ))
        } else {
            Ok(truncated)
        }
    }
}

// ---------------------------------------------------------------------------
// http_request
// ---------------------------------------------------------------------------

pub struct HttpRequestTool;

#[async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }

    fn description(&self) -> &str {
        "Send a raw HTTP request (GET/POST/PUT/PATCH/DELETE/HEAD). Pass headers\n\
         as an object, and a payload via 'json' (object) or 'body' (string).\n\
         Returns status code and response body. Non-GET/HEAD methods require\n\
         approval in interactive sessions. URLs that resolve to loopback,\n\
         private, or link-local addresses are refused."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "method": { "type": "string", "default": "GET", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"], "description": "HTTP method." },
                "url": { "type": "string", "description": "Full URL including scheme." },
                "headers": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Extra request headers as a string->string map." },
                "json": { "type": "object", "description": "JSON payload to send as the request body." },
                "body": { "type": "string", "description": "Raw string body (used when json is not given)." },
                "timeout_ms": { "type": "integer", "default": 30000, "description": "Request timeout in milliseconds." }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: &Value, _ctx: &ToolContext) -> Result<String> {
        let url = input
            .get("url")
            .and_then(|v| v.as_str())
            .context("missing 'url'")?;
        assert_public_url(url).await?;
        let method = input
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET")
            .to_uppercase();
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| anyhow::anyhow!("invalid method: {}", method))?;
        let timeout_ms = input
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(30000);

        let client = reqwest::Client::builder()
            .user_agent("jancode-agent/0.1")
            .timeout(std::time::Duration::from_millis(timeout_ms))
            .build()?;

        let mut req = client.request(method.clone(), url);
        if let Some(headers) = input.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in headers {
                if let Some(val) = v.as_str() {
                    req = req.header(k.clone(), val);
                }
            }
        }
        if let Some(j) = input.get("json") {
            req = req.json(j);
        } else if let Some(b) = input.get("body").and_then(|v| v.as_str()) {
            req = req.body(b.to_string());
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("{} {}", method, url))?;
        let status = resp.status();
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.text().await.unwrap_or_default();
        let truncated: String = body.chars().take(40000).collect();
        let header_line = if ct.is_empty() {
            String::new()
        } else {
            format!("content-type: {}\n", ct)
        };
        if truncated.len() < body.len() {
            Ok(format!(
                "HTTP {} {}\n{}{}\n\n[truncated: {} bytes total]",
                status.as_u16(),
                status.canonical_reason().unwrap_or(""),
                header_line,
                truncated,
                body.len()
            ))
        } else {
            Ok(format!(
                "HTTP {} {}\n{}{}",
                status.as_u16(),
                status.canonical_reason().unwrap_or(""),
                header_line,
                truncated
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// note
// ---------------------------------------------------------------------------

pub struct NoteTool;

#[async_trait]
impl Tool for NoteTool {
    fn name(&self) -> &str {
        "note"
    }

    fn description(&self) -> &str {
        "Maintain persistent freeform notes in .jancode-notes.md in the working\n\
         directory. Actions: 'create' (overwrite with content, optional title),\n\
         'append' (add content), 'show' (print the file), 'clear' (empty it).\n\
         Use it to record decisions, requirements, links, or progress that must\n\
         survive across sessions — read your notes at the start of a task."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["create", "append", "show", "clear"], "description": "What to do." },
                "title": { "type": "string", "description": "Optional section heading for create/append." },
                "content": { "type": "string", "description": "Note body for create/append." }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .context("missing 'action'")?;
        let file = ctx.working_dir.join(".jancode-notes.md");
        match action {
            "create" => {
                let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let mut text = String::new();
                if let Some(t) = input.get("title").and_then(|v| v.as_str()) {
                    if !t.is_empty() {
                        text.push_str(&format!("## {}\n\n", t));
                    }
                }
                text.push_str(content);
                tokio::fs::write(&file, text)
                    .await
                    .with_context(|| format!("writing {}", file.display()))?;
                Ok("notes updated (create)".to_string())
            }
            "append" => {
                let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let mut text = tokio::fs::read_to_string(&file).await.unwrap_or_default();
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                if let Some(t) = input.get("title").and_then(|v| v.as_str()) {
                    if !t.is_empty() {
                        text.push_str(&format!("\n## {}\n\n", t));
                    }
                }
                text.push_str(content);
                tokio::fs::write(&file, text)
                    .await
                    .with_context(|| format!("writing {}", file.display()))?;
                Ok("notes updated (append)".to_string())
            }
            "show" => {
                let text = tokio::fs::read_to_string(&file)
                    .await
                    .with_context(|| format!("notes are empty: {}", file.display()))?;
                Ok(text)
            }
            "clear" => {
                tokio::fs::write(&file, "")
                    .await
                    .with_context(|| format!("writing {}", file.display()))?;
                Ok("notes cleared".to_string())
            }
            _ => Ok(format!("unknown note action: {}", action)),
        }
    }
}

// ---------------------------------------------------------------------------
// docker
// ---------------------------------------------------------------------------

pub struct DockerTool;

const DOCKER_ACTIONS: &str =
    "ps, images, logs, inspect, stats, exec, run, build, stop, rm, pull, compose";

#[async_trait]
impl Tool for DockerTool {
    fn name(&self) -> &str {
        "docker"
    }

    fn description(&self) -> &str {
        "Manage local docker containers/images. Actions:\n\
         - ps: list containers; images: list images; stats: live usage\n\
         - logs: show container logs (--tail); inspect: container details\n\
         - exec: run a command inside a container (container + command)\n\
         - run: start a container (image, name, ports, detach, command)\n\
         - build: docker build -t tag path\n\
         - stop: stop a container; rm: remove a container; pull: pull an image\n\
         - compose: docker compose <command> (e.g. up -d, down, ps, logs)\n\
         State-changing actions (exec/run/build/stop/rm/pull/compose) require\n\
         approval; read-only actions run freely."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["ps", "images", "logs", "inspect", "stats", "exec", "run", "build", "stop", "rm", "pull", "compose"], "description": "docker operation." },
                "container": { "type": "string", "description": "Container name or id (logs/inspect/exec/stop/rm)." },
                "image": { "type": "string", "description": "Image name (run/pull)." },
                "command": { "type": "string", "description": "Command to run inside the container (exec) or the container command (run)." },
                "command_args": { "type": "array", "items": { "type": "string" }, "description": "Explicit argv for the container command (run); preferred over 'command' because it preserves quoting." },
                "name": { "type": "string", "description": "Container name to assign (run, --name)." },
                "ports": { "type": "string", "description": "Port mapping for run, e.g. '8080:80'." },
                "detach": { "type": "boolean", "description": "Run container in the background (run, -d)." },
                "remove": { "type": "boolean", "description": "Remove the container when it exits (run, --rm)." },
                "tag": { "type": "string", "description": "Image tag for build (-t)." },
                "build_path": { "type": "string", "description": "Build context path (build, default '.')." },
                "force": { "type": "boolean", "description": "Force removal (rm, -f) or compose recreate." },
                "tail": { "type": "integer", "description": "Log lines to show (logs, default 50)." },
                "compose_command": { "type": "string", "description": "Sub-command for compose, e.g. 'up -d', 'down', 'ps', 'logs' (default 'ps')." }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .context("missing 'action'")?;
        let arg = |name: &str| -> Result<Vec<String>, anyhow::Error> {
            let v = input
                .get(name)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            Ok(v.map(|s| s.to_string()).into_iter().collect())
        };

        let args: Vec<String> = match action {
            "ps" => vec!["ps".into(), "-a".into()],
            "images" => vec!["images".into()],
            "stats" => vec!["stats".into(), "--no-stream".into()],
            "logs" => {
                let mut a = vec!["logs".into(), "--tail".into()];
                let tail = input
                    .get("tail")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50)
                    .to_string();
                a.push(tail);
                let container = arg("container")?;
                if container.is_empty() {
                    return Ok("docker logs: missing 'container'".to_string());
                }
                a.extend(container);
                a
            }
            "inspect" => {
                let container = arg("container")?;
                if container.is_empty() {
                    return Ok("docker inspect: missing 'container'".to_string());
                }
                vec!["inspect".into(), container[0].clone()]
            }
            "exec" => {
                let container = arg("container")?;
                let command = arg("command")?;
                if container.is_empty() || command.is_empty() {
                    return Ok("docker exec: need 'container' and 'command'".to_string());
                }
                vec![
                    "exec".into(),
                    container[0].clone(),
                    "sh".into(),
                    "-lc".into(),
                    command[0].clone(),
                ]
            }
            "run" => {
                let image = arg("image")?;
                if image.is_empty() {
                    return Ok("docker run: missing 'image'".to_string());
                }
                let mut a: Vec<String> = vec!["run".into()];
                if input
                    .get("detach")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    a.push("-d".into());
                }
                if input
                    .get("remove")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    a.push("--rm".into());
                }
                if let Some(name) = input
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    a.push("--name".into());
                    a.push(name.to_string());
                }
                if let Some(ports) = input
                    .get("ports")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    a.push("-p".into());
                    a.push(ports.to_string());
                }
                a.push(image[0].clone());
                // Prefer an explicit argv array (quotes preserved); fall back to
                // the legacy whitespace-split string form.
                if let Some(arr) = input.get("command_args").and_then(|v| v.as_array()) {
                    a.extend(arr.iter().filter_map(|s| s.as_str().map(|s| s.to_string())));
                } else if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                    if !cmd.trim().is_empty() {
                        a.extend(cmd.split_whitespace().map(|s| s.to_string()));
                    }
                }
                a
            }
            "build" => {
                let tag = input
                    .get("tag")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                let path = input
                    .get("build_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".")
                    .to_string();
                let mut a = vec!["build".into()];
                if let Some(t) = tag {
                    a.push("-t".into());
                    a.push(t.to_string());
                }
                a.push(path);
                a
            }
            "stop" => {
                let container = arg("container")?;
                if container.is_empty() {
                    return Ok("docker stop: missing 'container'".to_string());
                }
                vec!["stop".into(), container[0].clone()]
            }
            "rm" => {
                let container = arg("container")?;
                if container.is_empty() {
                    return Ok("docker rm: missing 'container'".to_string());
                }
                let mut a = vec!["rm".into()];
                if input
                    .get("force")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    a.push("-f".into());
                }
                a.push(container[0].clone());
                a
            }
            "pull" => {
                let image = arg("image")?;
                if image.is_empty() {
                    return Ok("docker pull: missing 'image'".to_string());
                }
                vec!["pull".into(), image[0].clone()]
            }
            "compose" => {
                // `compose_command` is a string ("up -d"); split it so docker
                // receives `up` and `-d` as separate argv entries.
                let sub = input
                    .get("compose_command")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("ps");
                let mut a = vec!["compose".into()];
                a.extend(sub.split_whitespace().map(|s| s.to_string()));
                a
            }
            _ => {
                return Ok(format!(
                    "unknown docker action: {} (available: {})",
                    action, DOCKER_ACTIONS
                ))
            }
        };

        DockerTool::run_docker(ctx, args).await
    }
}

impl DockerTool {
    async fn run_docker(ctx: &ToolContext, args: Vec<String>) -> Result<String> {
        run_cli("docker", ctx, args, 120, &[]).await
    }
}

// ---------------------------------------------------------------------------
// sql
// ---------------------------------------------------------------------------

pub struct SqlTool;

#[async_trait]
impl Tool for SqlTool {
    fn name(&self) -> &str {
        "sql"
    }

    fn description(&self) -> &str {
        "Run a SQL query against a database. 'db' is optional and can be:\n\
         - a sqlite file path (e.g. 'app.db' or 'sqlite:/abs/path.db')\n\
         - a postgres://user:pass@host/db URL (uses the psql binary)\n\
         - a mysql://user:pass@host/db URL (uses the mysql binary)\n\
         When 'db' is omitted the config [database] url is used. Read-only\n\
         queries (SELECT/WITH/SHOW/EXPLAIN/DESCRIBE/PRAGMA) run freely; any\n\
         other statement (INSERT/UPDATE/DELETE/DDL) requires approval."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The SQL statement to execute." },
                "db": { "type": "string", "description": "Connection target (sqlite path, postgres://, or mysql:// URL). Defaults to config [database] url." }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .context("missing 'query'")?;
        let db = match input.get("db").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            Some(d) => d.to_string(),
            None if !ctx.database_url.is_empty() => ctx.database_url.clone(),
            None => return Ok("sql: no database configured — pass 'db' (sqlite path, postgres://, or mysql:// URL) or set [database] url in config.toml".to_string()),
        };

        let trim_query = query.trim();
        let mut split_at = trim_query.len();
        for (i, ch) in trim_query.char_indices() {
            if ch == ';' {
                split_at = i;
                break;
            }
        }
        let first_stmt = &trim_query[..split_at];

        let output = if db.starts_with("postgres://") || db.starts_with("postgresql://") {
            let args = vec![
                "-X".to_string(),
                "-q".to_string(),
                "-A".to_string(),
                "-F".to_string(),
                "|".to_string(),
                "-c".to_string(),
                first_stmt.to_string(),
                db,
            ];
            run_cli("psql", ctx, args, 30, &[]).await?
        } else if db.starts_with("mysql://") {
            Self::run_mysql(ctx, &db, first_stmt).await?
        } else {
            let path = db.strip_prefix("sqlite:").unwrap_or(&db);
            let full = ctx.resolve_path(path);
            let args = vec![
                "-header".to_string(),
                "-separator".to_string(),
                "|".to_string(),
                full.display().to_string(),
                first_stmt.to_string(),
            ];
            run_cli("sqlite3", ctx, args, 30, &[]).await?
        };

        let truncated: String = output.chars().take(20000).collect();
        if truncated.len() < output.len() {
            Ok(format!(
                "{}\n\n[truncated: {} bytes total]",
                truncated,
                output.len()
            ))
        } else {
            Ok(truncated)
        }
    }
}

impl SqlTool {
    async fn run_mysql(ctx: &ToolContext, url: &str, stmt: &str) -> Result<String> {
        let (host, user, pass, db) = parse_mysql_url(url).ok_or_else(|| {
            anyhow::anyhow!("invalid mysql URL — expected mysql://user:pass@host[:port]/dbname")
        })?;
        if db.is_empty() {
            return Ok(
                "sql: mysql URL needs a database name (mysql://user:pass@host/db)".to_string(),
            );
        }
        let args = vec![
            "--batch".to_string(),
            "-h".to_string(),
            host,
            "-u".to_string(),
            user,
            "-e".to_string(),
            stmt.to_string(),
            db,
        ];
        let env: Vec<(&str, &str)> = if pass.is_empty() {
            Vec::new()
        } else {
            // MYSQL_PWD avoids exposing the password in the process list.
            vec![("MYSQL_PWD", pass.as_str())]
        };
        run_cli("mysql", ctx, args, 30, &env).await
    }
}

// ---------------------------------------------------------------------------
// jq
// ---------------------------------------------------------------------------

pub struct JqTool;

#[async_trait]
impl Tool for JqTool {
    fn name(&self) -> &str {
        "jq"
    }

    fn description(&self) -> &str {
        "Query JSON or JSONL with a jq filter. Provide 'path' (a file in the\n\
         working directory) or 'json' (inline text). Read-only."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "description": "jq filter, e.g. '.items[] | {name, size}'. Default '.'."
                },
                "path": {
                    "type": "string",
                    "description": "Path to a JSON or JSONL file (relative to the working directory)."
                },
                "json": {
                    "type": "string",
                    "description": "Inline JSON or JSONL text to query instead of reading a file."
                },
                "compact": {
                    "type": "boolean",
                    "description": "Compact one-line output (jq -c). Default true."
                },
                "raw": {
                    "type": "boolean",
                    "description": "Emit strings without JSON quotes (jq -r). Default false."
                }
            },
            "required": ["filter"]
        })
    }

    fn is_independent(&self) -> bool {
        true
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let filter = input.get("filter").and_then(|v| v.as_str()).unwrap_or(".");
        let compact = input
            .get("compact")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let raw = input.get("raw").and_then(|v| v.as_bool()).unwrap_or(false);

        let mut args: Vec<String> = Vec::new();
        if raw {
            args.push("-r".to_string());
        }
        if compact {
            args.push("-c".to_string());
        }
        args.push(filter.to_string());

        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let inline = input
            .get("json")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());

        let output = match (path, inline) {
            (Some(p), _) => {
                let full = ctx.resolve_path(p);
                args.push(full.display().to_string());
                run_cli("jq", ctx, args, 30, &[]).await?
            }
            (None, Some(text)) => run_cli_with_stdin("jq", ctx, args, text, 30, &[]).await?,
            (None, None) => return Ok("jq: provide 'path' or 'json'".to_string()),
        };

        let truncated: String = output.chars().take(20000).collect();
        if truncated.len() < output.len() {
            Ok(format!(
                "{}\n\n[truncated: {} bytes total]",
                truncated,
                output.len()
            ))
        } else {
            Ok(truncated)
        }
    }
}

// ---------------------------------------------------------------------------
// gh
// ---------------------------------------------------------------------------

const GH_ACTIONS: &[&str] = &[
    "pr_list",
    "pr_view",
    "pr_diff",
    "pr_create",
    "pr_merge",
    "pr_comment",
    "pr_close",
    "pr_reopen",
    "issue_list",
    "issue_view",
    "issue_create",
    "issue_comment",
    "run_list",
    "run_view",
    "run_rerun",
    "run_cancel",
    "repo_view",
    "search_issues",
    "search_prs",
    "api",
];

/// True when a `gh` action creates/modifies remote state (requires approval).
pub fn gh_is_mutating(action: &str) -> bool {
    GH_MUTATING_ACTIONS.contains(&action)
}

/// Actions that create/modify remote state and therefore require approval.
const GH_MUTATING_ACTIONS: &[&str] = &[
    "pr_create",
    "pr_merge",
    "pr_comment",
    "pr_close",
    "pr_reopen",
    "issue_create",
    "issue_comment",
    "run_rerun",
    "run_cancel",
    "api",
];

fn gh_argv(action: &str, extra: &[String]) -> Option<Vec<String>> {
    let head: &[&str] = match action {
        "pr_list" => &["pr", "list"],
        "pr_view" => &["pr", "view"],
        "pr_diff" => &["pr", "diff"],
        "pr_create" => &["pr", "create"],
        "pr_merge" => &["pr", "merge"],
        "pr_comment" => &["pr", "comment"],
        "pr_close" => &["pr", "close"],
        "pr_reopen" => &["pr", "reopen"],
        "issue_list" => &["issue", "list"],
        "issue_view" => &["issue", "view"],
        "issue_create" => &["issue", "create"],
        "issue_comment" => &["issue", "comment"],
        "run_list" => &["run", "list"],
        "run_view" => &["run", "view"],
        "run_rerun" => &["run", "rerun"],
        "run_cancel" => &["run", "cancel"],
        "repo_view" => &["repo", "view"],
        "search_issues" => &["search", "issues"],
        "search_prs" => &["search", "prs"],
        "api" => &["api"],
        _ => return None,
    };
    let mut argv: Vec<String> = head.iter().map(|s| s.to_string()).collect();
    argv.extend(extra.iter().cloned());
    Some(argv)
}

pub struct GhTool;

#[async_trait]
impl Tool for GhTool {
    fn name(&self) -> &str {
        "gh"
    }

    fn description(&self) -> &str {
        "Run the GitHub CLI (gh): pull requests, issues, actions runs, and repo\n\
         queries. Read actions run freely; create/merge/comment/rerun require\n\
         approval. Requires gh installed and authenticated."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": GH_ACTIONS,
                    "description": "GitHub CLI operation to run."
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Extra gh flags/arguments (e.g. [\"123\", \"--json\", \"title,number\"])."
                },
                "repo": {
                    "type": "string",
                    "description": "Optional 'OWNER/REPO' target (adds --repo)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let mut extra: Vec<String> = input
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(repo) = input
            .get("repo")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            if action != "api" {
                extra.push("--repo".to_string());
                extra.push(repo.to_string());
            }
        }

        let argv = match gh_argv(action, &extra) {
            Some(a) => a,
            None => {
                return Ok(format!(
                    "gh: unknown action '{}' (available: {})",
                    action,
                    GH_ACTIONS.join(", ")
                ))
            }
        };
        let output = run_cli("gh", ctx, argv, 60, &[]).await?;
        let truncated: String = output.chars().take(20000).collect();
        if truncated.len() < output.len() {
            Ok(format!(
                "{}\n\n[truncated: {} bytes total]",
                truncated,
                output.len()
            ))
        } else {
            Ok(truncated)
        }
    }
}

// ---------------------------------------------------------------------------
// test
// ---------------------------------------------------------------------------

pub struct TestTool;

/// Condense test output: list failing test lines, then the tail. Keeps the
/// model's context small on large suites while preserving the summary.
fn summarize_test_output(output: &str) -> String {
    const MAX_TAIL: usize = 4000;
    let failed: Vec<String> = output
        .lines()
        .filter(|l| l.contains("FAILED") || l.contains("... FAILED"))
        .map(|l| l.trim().to_string())
        .take(25)
        .collect();

    let mut out = String::new();
    if !failed.is_empty() {
        out.push_str("FAILING:\n");
        for f in &failed {
            out.push_str("  ");
            out.push_str(f);
            out.push('\n');
        }
        out.push('\n');
    }

    let total_chars = output.chars().count();
    let tail: String = output
        .chars()
        .rev()
        .take(MAX_TAIL)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    out.push_str(&tail);
    if total_chars > MAX_TAIL {
        out.push_str(&format!("\n[truncated: {} bytes total]", output.len()));
    }
    out
}

#[async_trait]
impl Tool for TestTool {
    fn name(&self) -> &str {
        "test"
    }

    fn description(&self) -> &str {
        "Run the project's test suite and return a condensed summary (failing\n\
         tests + tail) instead of raw output. Runners: cargo, npm, pnpm, yarn,\n\
         pytest, go, custom. Known runners are ungated; 'custom' requires\n\
         approval."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "runner": {
                    "type": "string",
                    "enum": ["cargo", "npm", "pnpm", "yarn", "pytest", "go", "custom"],
                    "description": "Test runner to invoke."
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Extra arguments passed to the runner."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds (default 300)."
                },
                "parse": {
                    "type": "boolean",
                    "description": "Condense the output (default true)."
                }
            },
            "required": ["runner"]
        })
    }

    async fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<String> {
        let runner = input
            .get("runner")
            .and_then(|v| v.as_str())
            .unwrap_or("cargo");
        let extra: Vec<String> = input
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let timeout = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(300);
        let parse = input.get("parse").and_then(|v| v.as_bool()).unwrap_or(true);

        let (exe, argv): (String, Vec<String>) = match runner {
            "cargo" => {
                let mut a = vec!["test".to_string()];
                a.extend(extra);
                ("cargo".to_string(), a)
            }
            "npm" => {
                let mut a = vec!["test".to_string(), "--".to_string()];
                a.extend(extra);
                ("npm".to_string(), a)
            }
            "pnpm" | "yarn" => {
                let mut a = vec!["test".to_string()];
                a.extend(extra);
                (runner.to_string(), a)
            }
            "pytest" => {
                if extra.is_empty() {
                    ("pytest".to_string(), vec!["-q".to_string()])
                } else {
                    ("pytest".to_string(), extra)
                }
            }
            "go" => {
                let mut a = vec!["test".to_string()];
                if extra.is_empty() {
                    a.push("./...".to_string());
                } else {
                    a.extend(extra);
                }
                ("go".to_string(), a)
            }
            "custom" => {
                if extra.is_empty() {
                    return Ok(
                        "test: 'custom' needs an args array, e.g. [\"make\",\"test\"]".to_string(),
                    );
                }
                let mut it = extra.into_iter();
                let exe = it.next().unwrap_or_default();
                (exe, it.collect())
            }
            other => {
                return Ok(format!(
                    "test: unknown runner '{}' (cargo|npm|pnpm|yarn|pytest|go|custom)",
                    other
                ))
            }
        };

        let output = run_cli(&exe, ctx, argv, timeout, &[]).await?;
        if parse {
            Ok(summarize_test_output(&output))
        } else {
            Ok(output)
        }
    }
}

fn parse_mysql_url(url: &str) -> Option<(String, String, String, String)> {
    let rest = url.strip_prefix("mysql://")?;
    let (auth, hostport) = match rest.split_once('@') {
        Some((a, h)) => (a, h),
        None => (rest, rest),
    };
    let (user, pass) = match auth.split_once(':') {
        Some((u, p)) => (u.to_string(), p.to_string()),
        None => (auth.to_string(), String::new()),
    };
    let (hostpart, db) = match hostport.split_once('/') {
        Some((h, d)) => (h, d.split('?').next().unwrap_or("").to_string()),
        None => (hostport, String::new()),
    };
    let host = hostpart.split(':').next().unwrap_or(hostpart).to_string();
    Some((host, user, pass, db))
}

/// Heuristic: is this statement read-only (approval-free)?
///
/// `WITH` is only treated as read-only when the statement contains no
/// data-modifying keyword (`INSERT`/`UPDATE`/`DELETE`/`MERGE`); Postgres and
/// MySQL allow `WITH x AS (INSERT ...) SELECT ...`, which mutates.
pub fn sql_is_read_only(query: &str) -> bool {
    let q = query.trim_start().to_uppercase();
    if ["SELECT", "SHOW", "EXPLAIN", "DESCRIBE", "DESC", "PRAGMA"]
        .iter()
        .any(|kw| q.starts_with(kw))
    {
        return true;
    }
    if q.starts_with("WITH") {
        return !contains_write_keyword(query);
    }
    false
}

/// True when the query contains a data-modifying keyword outside of quoted
/// string literals. Heuristic, but errs toward gating (safe direction).
fn contains_write_keyword(query: &str) -> bool {
    let re = regex::Regex::new(r"(?i)\b(insert|update|delete|merge)\b").unwrap();
    let mut stripped = String::with_capacity(query.len());
    let mut in_single = false;
    let mut in_double = false;
    let chars = query.chars();
    for c in chars {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ if in_single || in_double => stripped.push(' '),
            _ => stripped.push(c),
        }
    }
    re.is_match(&stripped)
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
        assert!(is_outside(base, Path::new("/etc/passwd")));
        assert!(!is_outside(base, Path::new("/work/proj")));
    }

    #[test]
    fn bash_escape_detection() {
        let base = Path::new("/work/proj");
        // Safe: in-workspace commands.
        assert!(bash_escapes_workspace("ls", base, "basic").is_none());
        assert!(bash_escapes_workspace("cargo test", base, "basic").is_none());
        assert!(bash_escapes_workspace("grep -r alpha src", base, "basic").is_none());
        assert!(bash_escapes_workspace("cat .env", base, "basic").is_none());
        assert!(bash_escapes_workspace("docker ps -a", base, "basic").is_none());
        assert!(bash_escapes_workspace("curl -s http://localhost:5000", base, "basic").is_none());
        // Escapes: absolute paths, home, `..`, `cd` out.
        assert!(bash_escapes_workspace("cat /etc/hosts", base, "basic").is_some());
        assert!(bash_escapes_workspace("ls ~/sysadmin-mcp", base, "basic").is_some());
        assert!(bash_escapes_workspace("cat $HOME/.claude/settings.json", base, "basic").is_some());
        assert!(bash_escapes_workspace("cd .. && ls", base, "basic").is_some());
        assert!(bash_escapes_workspace("cd /tmp && pwd", base, "basic").is_some());
        assert!(
            bash_escapes_workspace("cd ~/sysadmin-mcp && grep -r alpha .", base, "basic").is_some()
        );
        assert!(bash_escapes_workspace("grep -r alpha ../secrets", base, "basic").is_some());
        assert!(bash_escapes_workspace("ls $PWD/../..", base, "basic").is_some());
        // "off" never gates.
        assert!(bash_escapes_workspace("cat /etc/hosts", base, "off").is_none());
        assert!(bash_escapes_workspace("ls ~/sysadmin-mcp", base, "off").is_none());
        // "strict" gates on any cd / $PWD / $OLDPWD.
        assert!(bash_escapes_workspace("cd src && ls", base, "strict").is_some());
        assert!(bash_escapes_workspace("echo $PWD", base, "strict").is_some());
        assert!(bash_escapes_workspace("echo $OLDPWD", base, "strict").is_some());
        // "strict" still allows plain in-workspace commands.
        assert!(bash_escapes_workspace("ls", base, "strict").is_none());
        assert!(bash_escapes_workspace("cargo test", base, "strict").is_none());
        // Heredoc bodies are data, not paths — must NOT be flagged for PATH escapes
        // even if they contain `/*` comments or absolute-looking text. But a
        // heredoc that WRITES a file (`cat > file` / `cat >> file`) is still a
        // file mutation and IS gated.
        assert!(bash_escapes_workspace("cat >> styles.css << 'EOF'\n/* ==== Cart ==== */\n.cart-overlay { display: none }\nEOF", base, "basic").is_some());
        assert!(bash_escapes_workspace(
            "cat > file.txt <<EOF\n/usr/share/notes\nEOF",
            base,
            "basic"
        )
        .is_some());
        // A bare `/` or comment markers are not escapes.
        assert!(bash_escapes_workspace("ls /", base, "basic").is_none());
        assert!(bash_escapes_workspace("echo /* comment */", base, "basic").is_none());
        // `which`/`ls` on system paths is a read, but still an absolute path —
        // keep gating it (it's a genuine outside-workspace read).
        assert!(bash_escapes_workspace("ls /usr/bin/node*", base, "basic").is_some());
    }

    #[test]
    fn bash_escape_workspace_aware() {
        let base = Path::new("/home/user/proj");
        // cd to the workspace itself (absolute) is NOT a path escape, BUT the
        // `sed -i` mutation still requires approval.
        assert!(
            bash_escapes_workspace("cd /home/user/proj && sed -i s/a/b/ x.css", base, "basic")
                .is_some()
        );
        // cd to a subdir inside the workspace is NOT an escape (basic mode).
        assert!(bash_escapes_workspace("cd src && cargo build", base, "basic").is_none());
        // `sed -i` on a file inside the workspace is a MUTATION -> gated.
        assert!(
            bash_escapes_workspace("sed -i s/a/b/ /home/user/proj/x.css", base, "basic").is_some()
        );
        // Read-only sed (no -i) inside workspace is not a mutation.
        assert!(
            bash_escapes_workspace("sed -n '1,5p' /home/user/proj/x.css", base, "basic").is_none()
        );
        // cd to another project outside the workspace IS an escape.
        assert!(bash_escapes_workspace("cd /home/user/other && ls", base, "basic").is_some());
        // cd .. is still an escape.
        assert!(bash_escapes_workspace("cd .. && ls", base, "basic").is_some());
        // cd to absolute path OUTSIDE workspace is still an escape.
        assert!(bash_escapes_workspace("cd /tmp && pwd", base, "basic").is_some());
        // strict still gates even an in-workspace cd.
        assert!(bash_escapes_workspace("cd src && cargo build", base, "strict").is_some());
    }

    #[test]
    fn mutation_targets_extraction() {
        assert_eq!(
            mutation_targets("sed -i 's/a/b/g' styles.css", "sed"),
            vec!["styles.css".to_string()]
        );
        assert_eq!(
            mutation_targets("sed -i s/a/b/ src/main.rs", "sed"),
            vec!["src/main.rs".to_string()]
        );
        assert_eq!(
            mutation_targets("rm -rf target", "rm"),
            Vec::<String>::new()
        ); // "target" looks like a name, no ext/slash -> empty
        assert_eq!(
            mutation_targets("touch a.css b.js", "touch"),
            vec!["a.css".to_string(), "b.js".to_string()]
        );
        assert_eq!(
            mutation_targets("cat > out.txt << EOF", "shell-redirect"),
            vec!["out.txt".to_string()]
        );
        assert_eq!(
            mutation_targets("cd /x/y && sed -i s/a/b/ f.css", "sed"),
            vec!["f.css".to_string()]
        );
    }

    #[test]
    fn bash_mutation_detection() {
        // In-place editors and file writers mutate.
        assert_eq!(bash_mutation("sed -i s/a/b/ styles.css"), Some("sed"));
        assert_eq!(bash_mutation("perl -pi -e s/a/b/ f.rs"), Some("perl"));
        assert_eq!(bash_mutation("perl -i -pe s/a/b/ f.rs"), Some("perl"));
        assert_eq!(bash_mutation("sed -ni '1,5p' f.rs"), Some("sed"));
        assert_eq!(
            bash_mutation("sed --in-place=.bak s/a/b/ f.rs"),
            Some("sed")
        );
        assert_eq!(bash_mutation("rm -rf target"), Some("rm"));
        assert_eq!(bash_mutation("tee out.log"), Some("tee"));
        assert_eq!(bash_mutation("cat > out.txt"), Some("shell-redirect"));
        assert_eq!(bash_mutation("echo hi >> note.md"), Some("shell-redirect"));
        // Read-only commands must NOT report a mutation, so the client never
        // pays for a post-hoc `git diff`.
        assert_eq!(bash_mutation("git status"), None);
        assert_eq!(bash_mutation("cargo test"), None);
        // A non-in-place sed only prints to stdout.
        assert_eq!(bash_mutation("sed -n '1,5p' file.rs"), None);
        assert_eq!(bash_mutation("grep -r alpha src"), None);
    }

    #[test]
    fn bash_mutation_targets_scope_the_diff() {
        // The client scopes its diff to these paths, so they must match what
        // the approval gate names.
        assert_eq!(
            bash_mutation_targets("sed -i s/a/b/ styles.css"),
            Some(vec!["styles.css".to_string()])
        );
        assert_eq!(
            bash_mutation_targets("cat > out.txt << EOF"),
            Some(vec!["out.txt".to_string()])
        );
        // Read-only commands yield None, not an empty list, so callers can
        // short-circuit.
        assert_eq!(bash_mutation_targets("git status"), None);
        assert_eq!(bash_mutation_targets("sed -n '1,5p' file.rs"), None);
    }

    #[test]
    fn sed_targets_survive_real_world_invocations() {
        // The shape claude/copilot-style agents actually emit for a palette
        // swap: multi-line, backslash-continued, several -e expressions, and
        // expressions containing spaces and slashes.
        let multiline = "sed -i \\\n -e 's/#4a2e1b/#2f4238/g' \\\n -e 's/rgba(43, 29, 19/ rgba(20, 52, 43/g' \\\n styles.css";
        assert_eq!(
            bash_mutation_targets(multiline),
            Some(vec!["styles.css".to_string()]),
            "backslash continuations must still yield the file operand"
        );
        // Several operands are all reported.
        assert_eq!(
            bash_mutation_targets("sed -i -e 's/a/b/' a.css b.css"),
            Some(vec!["a.css".to_string(), "b.css".to_string()])
        );
        // A trailing shell chain still isolates the file operands.
        assert_eq!(
            bash_mutation_targets("sed -i s/a/b/ styles.css && grep -n color styles.css"),
            Some(vec!["styles.css".to_string()])
        );
    }

    #[test]
    fn normalize_bash_cd_behavior() {
        let base = Path::new("/home/user/proj");
        // Absolute cd to the workspace root is rewritten to a relative `cd .`,
        // preserving the directory change but removing the absolute path.
        assert_eq!(
            normalize_bash_cd("cd /home/user/proj && sed -i s/a/b/ styles.css", base),
            Some("cd . && sed -i s/a/b/ styles.css".to_string())
        );
        // Absolute cd to a workspace subdir is rewritten to a relative path.
        assert_eq!(
            normalize_bash_cd("cd /home/user/proj/src && make", base),
            Some("cd src && make".to_string())
        );
        // Already-relative cd is unchanged (None = no rewrite needed).
        assert_eq!(normalize_bash_cd("cd src && cargo build", base), None);
        // Absolute cd to the workspace root alone -> `cd .`.
        assert_eq!(
            normalize_bash_cd("cd /home/user/proj", base),
            Some("cd .".to_string())
        );
        // cd OUTSIDE the workspace is NOT rewritten (gate must still see it).
        assert_eq!(normalize_bash_cd("cd /home/user/other && ls", base), None);
        // absolute path outside -> not rewritten.
        assert_eq!(normalize_bash_cd("cd /tmp && pwd", base), None);
        // no leading cd -> not rewritten.
        assert_eq!(normalize_bash_cd("sed -i s/a/b/ styles.css", base), None);
        // ~ / $HOME -> not rewritten (gate handles).
        assert_eq!(normalize_bash_cd("cd ~/notes && ls", base), None);
        // empty / `cd` alone -> not rewritten.
        assert_eq!(normalize_bash_cd("", base), None);
        assert_eq!(normalize_bash_cd("cd", base), None);
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

    #[test]
    fn sql_read_only_detection() {
        assert!(sql_is_read_only("SELECT * FROM users"));
        assert!(sql_is_read_only("  select id from users limit 5"));
        assert!(sql_is_read_only(
            "WITH recent AS (SELECT 1) SELECT * FROM recent"
        ));
        assert!(sql_is_read_only("SHOW TABLES"));
        assert!(sql_is_read_only("EXPLAIN SELECT 1"));
        assert!(sql_is_read_only("PRAGMA table_info(users)"));
        assert!(!sql_is_read_only("INSERT INTO users VALUES (1)"));
        assert!(!sql_is_read_only("update users set name='x'"));
        assert!(!sql_is_read_only("DELETE FROM users"));
        assert!(!sql_is_read_only("CREATE TABLE t (id int)"));
        assert!(!sql_is_read_only("DROP TABLE users"));
    }

    #[test]
    fn independent_tool_flags() {
        let reg = default_registry();
        // Read-only tools are safe to run concurrently.
        for name in ["read", "list_dir", "glob", "agentgrep", "fetch_url"] {
            let t = reg.find(name).expect(name);
            assert!(t.is_independent(), "{} should be independent", name);
        }
        // Mutating / stateful tools must NOT be parallelized.
        for name in [
            "write",
            "edit",
            "apply_patch",
            "bash",
            "git",
            "docker",
            "sql",
            "plan",
            "note",
            "http_request",
        ] {
            let t = reg.find(name).expect(name);
            assert!(!t.is_independent(), "{} should NOT be independent", name);
        }
    }

    #[test]
    fn write_tools_enforce_workspace_confinement() {
        let ctx = ToolContext::new(
            Path::new("/work/proj").to_path_buf(),
            String::new(),
            "basic".to_string(),
        );
        assert!(resolve_within(&ctx, "src/main.rs").is_ok());
        assert!(resolve_within(&ctx, "./src/../src/main.rs").is_ok());
        assert!(resolve_within(&ctx, "/work/proj/src/main.rs").is_ok());
        assert!(resolve_within(&ctx, "../outside.txt").is_err());
        assert!(resolve_within(&ctx, "/etc/passwd").is_err());
        assert!(resolve_within(&ctx, "src/../../outside.txt").is_err());
        // An explicit interactive approval unlocks exactly that path.
        allow_outside(&ctx, "../outside.txt");
        assert!(resolve_within(&ctx, "../outside.txt").is_ok());
        assert!(resolve_within(&ctx, "/etc/passwd").is_err());
    }

    #[test]
    fn builtin_tools_match_registry() {
        let reg = default_registry();
        let names: Vec<&str> = reg.all().iter().map(|t| t.name()).collect();
        assert_eq!(names, BUILTIN_TOOLS);
        for t in reg.all() {
            let def = t.to_definition();
            assert!(def.parameters.is_object(), "{} schema", def.name);
        }
    }

    #[test]
    fn patch_paths_lists_new_targets_only() {
        let patch =
            "--- a/one.rs\n+++ b/one.rs\n@@ -1 +1 @@\n-a\n+b\n--- a/gone.rs\n+++ /dev/null\n";
        assert_eq!(patch_paths(patch), vec!["one.rs".to_string()]);
    }

    #[test]
    fn with_is_not_read_only_when_it_writes() {
        assert!(sql_is_read_only("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(!sql_is_read_only(
            "WITH x AS (INSERT INTO t VALUES (1)) SELECT * FROM x"
        ));
        assert!(!sql_is_read_only(
            "WITH x AS (UPDATE t SET a=1) SELECT * FROM x"
        ));
        // A quoted string mentioning the keyword doesn't count.
        assert!(sql_is_read_only("SELECT 'update' AS note FROM t"));
    }

    #[test]
    fn non_global_address_detection() {
        use std::net::IpAddr;
        for s in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "fc00::1",
        ] {
            assert!(is_non_global_ip(&s.parse::<IpAddr>().unwrap()), "{}", s);
        }
        for s in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
            assert!(!is_non_global_ip(&s.parse::<IpAddr>().unwrap()), "{}", s);
        }
    }

    #[tokio::test]
    async fn jq_tool_queries_inline_json() {
        // Skip on hosts without jq (the tool itself requires it).
        if std::process::Command::new("jq")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let ctx = ToolContext::new(std::env::temp_dir(), String::new(), "basic".to_string());
        let out = JqTool
            .execute(
                &serde_json::json!({
                    "filter": ".items[] | .name",
                    "json": "{\"items\":[{\"name\":\"a\"},{\"name\":\"b\"}]}",
                    "raw": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out, "a\nb");
    }

    #[tokio::test]
    async fn test_tool_runs_custom_command_and_summarizes() {
        let ctx = ToolContext::new(std::env::temp_dir(), String::new(), "basic".to_string());
        let out = TestTool
            .execute(
                &serde_json::json!({
                    "runner": "custom",
                    "args": ["sh", "-c", "printf 'test result: ok. 1 passed; 0 failed\\n'"]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("test result: ok"));
        assert!(!out.contains("FAILING:"));
    }

    #[test]
    fn gh_action_mapping_and_gating() {
        assert_eq!(gh_argv("pr_list", &[]).unwrap(), vec!["pr", "list"]);
        assert_eq!(
            gh_argv("issue_view", &["42".to_string(), "--json".to_string()]).unwrap(),
            vec!["issue", "view", "42", "--json"]
        );
        assert!(gh_argv("nope", &[]).is_none());
        assert!(gh_is_mutating("pr_merge"));
        assert!(gh_is_mutating("api"));
        assert!(!gh_is_mutating("pr_list"));
        assert!(!gh_is_mutating("run_view"));
    }

    #[test]
    fn test_summary_lists_failures_and_tail() {
        let out = "running 3 tests\ntest alpha ... ok\ntest beta ... FAILED\ntest result: FAILED. 1 passed; 1 failed\n";
        let s = summarize_test_output(out);
        assert!(s.contains("FAILING:"));
        assert!(s.contains("test beta ... FAILED"));
        assert!(s.contains("test result: FAILED"));
    }

    #[test]
    fn test_summary_ok_output_has_no_failure_section() {
        let s = summarize_test_output("test result: ok. 3 passed; 0 failed\n");
        assert!(!s.contains("FAILING:"));
        assert!(s.contains("test result: ok"));
    }

    #[test]
    fn parses_mysql_url() {
        let (host, user, pass, db) =
            parse_mysql_url("mysql://alice:s3cret@db.internal:3306/appdb").unwrap();
        assert_eq!(host, "db.internal");
        assert_eq!(user, "alice");
        assert_eq!(pass, "s3cret");
        assert_eq!(db, "appdb");
        assert!(parse_mysql_url("mysql://:3306/nodb").is_some());
        assert!(parse_mysql_url("postgres://a:b@h/d").is_none());
    }
}
