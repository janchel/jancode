use std::path::Path;

/// Instruction files injected into the system prompt, following the
/// AGENTS.md / CLAUDE.md ecosystem convention.
const INSTRUCTION_FILENAMES: &[&str] = &["AGENTS.md", "agents.md", "CLAUDE.md", ".claude/CLAUDE.md"];

/// Collect project instructions for a working directory and render them as a
/// markdown block for the system prompt. Sources, in trust order (later
/// overrides earlier):
///   1. global `~/.config/jancode/AGENTS.md` (machine-wide rules),
///   2. every AGENTS.md/CLAUDE.md found walking up from `working_dir` to the
///      filesystem root — closest to the work wins.
/// Returns an empty string when nothing is found, so callers can skip the
/// system-prompt section cheaply.
pub fn load_instructions(working_dir: &str) -> String {
    let mut sections: Vec<(String, String)> = Vec::new();

    if let Some(config_dir) = dirs::config_dir() {
        let global = config_dir.join("jancode").join("AGENTS.md");
        if global.is_file() {
            if let Ok(text) = std::fs::read_to_string(&global) {
                sections.push((global.display().to_string(), text));
            }
        }
    }

    let mut chain: Vec<(String, String)> = Vec::new();
    let mut current = Path::new(working_dir).to_path_buf();
    loop {
        for name in INSTRUCTION_FILENAMES {
            let p = current.join(name);
            if p.is_file() {
                if let Ok(text) = std::fs::read_to_string(&p) {
                    chain.push((p.display().to_string(), text));
                }
            }
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    chain.reverse(); // parent-most first, closest file wins for conflicts
    sections.extend(chain);

    let mut out = String::new();
    for (path, text) in &sections {
        if !text.trim().is_empty() {
            out.push_str(&format!("\n===== {} =====\n{}\n", path, text));
        }
    }
    out
}