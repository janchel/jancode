use std::path::{Path, PathBuf};

/// Instruction files injected into the system prompt, following the
/// AGENTS.md / CLAUDE.md ecosystem convention.
const INSTRUCTION_FILENAMES: &[&str] = &[
    "AGENTS.md",
    "agents.md",
    "AGENT.md",
    "CLAUDE.md",
    "SKILLS.md",
    "skills.md",
    ".claude/CLAUDE.md",
    ".claude/AGENTS.md",
];

/// A discoverable skill: a `SKILL.md` inside a skill directory.
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: String,
}

/// Parse a SKILL.md into (name, description). Uses YAML frontmatter when
/// present (`name:` / `description:`), else the first markdown heading, else
/// the parent directory name.
fn parse_skill(path: &Path, text: &str) -> (String, String) {
    if let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    {
        let end = rest.find("\n---").or_else(|| rest.find("\r\n---"));
        if let Some(end) = end {
            let fm = &rest[..end];
            let mut name = None;
            let mut description = None;
            for line in fm.lines() {
                if let Some(v) = line.strip_prefix("name:") {
                    name = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
                } else if let Some(v) = line.strip_prefix("description:") {
                    description = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
                }
            }
            if let Some(n) = name.filter(|n| !n.is_empty()) {
                return (n, description.unwrap_or_default());
            }
        }
    }
    let name = text
        .lines()
        .find_map(|l| l.strip_prefix("# ").map(|s| s.trim().to_string()))
        .filter(|n| !n.is_empty())
        .or_else(|| {
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "skill".to_string());
    (name, String::new())
}

/// Candidate skill directories: workspace conventions plus user config dirs.
fn skill_dirs(working_dir: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let root = Path::new(working_dir);
    for sub in [
        "skills",
        ".claude/skills",
        ".kilo/skills",
        ".jancode/skills",
    ] {
        dirs.push(root.join(sub));
    }
    if let Some(config_dir) = dirs::config_dir() {
        dirs.push(config_dir.join("jancode").join("skills"));
        dirs.push(config_dir.join("kilo").join("skills"));
        dirs.push(config_dir.join("claude").join("skills"));
    }
    dirs
}

/// Discover skill directories (`<dir>/<name>/SKILL.md`) in the workspace and
/// the user's config dirs. Returns skills sorted by name.
pub fn load_skills(working_dir: &str) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for dir in skill_dirs(working_dir) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_file = path.join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            let canon = skill_file
                .canonicalize()
                .unwrap_or_else(|_| skill_file.clone());
            if !seen.insert(canon) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&skill_file) else {
                continue;
            };
            let (name, description) = parse_skill(&skill_file, &text);
            skills.push(Skill {
                name,
                description: description.chars().take(300).collect(),
                path: skill_file.display().to_string(),
            });
        }
    }
    skills.sort_by_key(|s| s.name.to_lowercase());
    skills
}

/// Render the skill index for the system prompt. The model reads a skill's
/// path with `read` when it matches the request, so only the index is injected.
pub fn render_skills(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n# Skills\nSkill files available in this environment. When the user's request matches a skill's description, read its path with `read` and follow it.\n",
    );
    for s in skills {
        if s.description.is_empty() {
            out.push_str(&format!("- {} ({})\n", s.name, s.path));
        } else {
            out.push_str(&format!("- {}: {} ({})\n", s.name, s.description, s.path));
        }
    }
    out
}

/// Collect project instructions for a working directory and render them as a
/// markdown block for the system prompt. Sources, in trust order (later
/// overrides earlier): the global `~/.config/jancode/AGENTS.md` (machine-wide
/// rules), then every AGENTS.md/CLAUDE.md found walking up from `working_dir`
/// to the filesystem root, with the closest file to the work winning.
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
    out.push_str(&render_skills(&load_skills(working_dir)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "jancode-agents-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parse_skill_frontmatter_and_fallback() {
        let (n, d) = parse_skill(
            Path::new("/x/foo/SKILL.md"),
            "---\nname: Review\ndescription: Do a review\n---\n# Review\n",
        );
        assert_eq!(n, "Review");
        assert_eq!(d, "Do a review");
        let (n, _) = parse_skill(Path::new("/x/bar/SKILL.md"), "# Bar\nbody\n");
        assert_eq!(n, "Bar");
        let (n, _) = parse_skill(Path::new("/x/baz/SKILL.md"), "no heading\n");
        assert_eq!(n, "baz");
    }

    #[test]
    fn discovers_skills_and_instruction_files() {
        let root = tempdir("skills");
        fs::create_dir_all(root.join("skills/foo")).unwrap();
        fs::write(
            root.join("skills/foo/SKILL.md"),
            "---\nname: Foo\ndescription: Foo skill\n---\n# Foo\n",
        )
        .unwrap();
        fs::create_dir_all(root.join(".claude/skills/bar")).unwrap();
        fs::write(root.join(".claude/skills/bar/SKILL.md"), "# Bar\n").unwrap();
        fs::write(root.join("AGENT.md"), "agent rules").unwrap();
        fs::write(root.join("SKILLS.md"), "root skills").unwrap();

        let skills = load_skills(root.to_str().unwrap());
        assert!(skills.iter().any(|s| s.name == "Foo"));
        assert!(skills.iter().any(|s| s.name == "Bar"));

        let text = load_instructions(root.to_str().unwrap());
        assert!(text.contains("agent rules"));
        assert!(text.contains("root skills"));
        assert!(text.contains("# Skills"));
        assert!(text.contains("Foo skill"));
        assert!(text.contains("skills/foo/SKILL.md"));
        let _ = fs::remove_dir_all(&root);
    }
}
