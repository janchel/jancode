use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::config::jancode_dir;

/// A single persisted memory note. Notes are scoped to a folder so they only
/// surface for the project they came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNote {
    pub id: String,
    pub text: String,
    pub folder: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

fn memory_path() -> PathBuf {
    jancode_dir().join("memory.json")
}

/// Load all memory notes. Missing/corrupt file is treated as empty.
pub fn load_notes() -> Result<Vec<MemoryNote>> {
    let path = memory_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&data).unwrap_or_default())
}

/// Persist the full note list atomically.
fn save_notes(notes: &[MemoryNote]) -> Result<()> {
    let path = memory_path();
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(notes)?;
    fs::write(&tmp, data).context("writing memory tmp")?;
    fs::rename(&tmp, &path).context("renaming memory file")?;
    Ok(())
}

/// Add a new memory note. If an identical note (same text, same folder) already
/// exists, its timestamp is refreshed instead of duplicating.
pub fn add_note(text: &str, folder: &str) -> Result<()> {
    let mut notes = load_notes()?;
    let now = Utc::now().timestamp_millis();
    if let Some(existing) = notes
        .iter_mut()
        .find(|n| n.folder == folder && n.text == text)
    {
        existing.updated_at_ms = now;
        return save_notes(&notes);
    }
    notes.push(MemoryNote {
        id: Uuid::new_v4().to_string(),
        text: text.to_string(),
        folder: folder.to_string(),
        created_at_ms: now,
        updated_at_ms: now,
    });
    save_notes(&notes)
}

/// Remove a note by id. Returns true if it was found and removed.
pub fn remove_note(id: &str) -> Result<bool> {
    let mut notes = load_notes()?;
    let before = notes.len();
    notes.retain(|n| n.id != id);
    if notes.len() == before {
        return Ok(false);
    }
    save_notes(&notes)?;
    Ok(true)
}

/// Automatically extract memory-worthy facts from a user message. Uses simple
/// heuristic markers (no embeddings). For every extracted sentence, persists a
/// note scoped to `folder`.
pub fn auto_capture(text: &str, folder: &str) -> Result<()> {
    for sentence in sentences(text) {
        if let Some(fact) = filter_fact(&sentence) {
            add_note(&fact, folder)?;
        }
    }
    Ok(())
}

/// Split text into sentences on common boundaries.
fn sentences(text: &str) -> Vec<String> {
    text.replace('\n', ". ")
        .split(". ")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Return the sentence if it looks like a durable fact worth remembering.
/// Matches preference/decision markers but excludes questions and file paths.
fn filter_fact(sentence: &str) -> Option<String> {
    let markers = [
        "always",
        "never",
        "prefer",
        "prefers",
        "we use",
        "i use",
        "i like",
        "i prefer",
        "uses ",
        "remember",
        "make sure",
        "be sure to",
        "in future",
        "going forward",
        "from now on",
        "convention",
        "in this project",
        "this project uses",
        "this project is",
        "this is a",
        "the project",
        "the codebase",
        "the app",
        "the server",
        "the database",
        "the api",
        "stack",
        "framework",
        "deployed",
        "deployment",
        "environment",
    ];
    let lower = sentence.to_lowercase();
    if sentence.starts_with('?') || sentence.ends_with('?') {
        return None;
    }
    // Skip short one-liners and file paths (a path + a command is not a durable fact).
    if sentence.len() < 12 {
        return None;
    }
    if sentence.contains('/') && !lower.contains("project") && !lower.contains("deploy") {
        return None;
    }
    if markers.iter().any(|m| lower.contains(m)) {
        Some(sentence.to_string())
    } else {
        None
    }
}

/// Retrieve the most relevant notes for a query, scoped to a folder. Uses a
/// simple token-overlap score (Dice coefficient). Notes from the active folder
/// are boosted so local context wins over global notes.
pub fn retrieve(query: &str, folder: &str, k: usize) -> Vec<MemoryNote> {
    let notes = load_notes().unwrap_or_default();
    rank_notes(&notes, query, folder, k)
}

/// Is a note saved in `note_folder` in scope for the current `folder`?
///
/// Scoped by directory, never by keywords alone. A note applies to the folder
/// it was saved in and to that folder's descendants (a note saved in `~/devops`
/// covers `~/devops/proj`), but never to a sibling project.
///
/// This gate matters: keyword overlap is far too weak on its own. Ordinary
/// English ("can you … this … and … project") scores above the relevance
/// threshold, so without a hard folder check a note from one project gets
/// injected into an unrelated one — and because the notes are appended to the
/// system prompt as facts to trust, the model then confidently acts on the
/// wrong project.
fn note_in_scope(note_folder: &str, folder: &str) -> bool {
    if note_folder.is_empty() || folder.is_empty() {
        return false;
    }
    if note_folder == folder {
        return true;
    }
    let note = Path::new(note_folder);
    Path::new(folder).ancestors().any(|ancestor| ancestor == note)
}

/// Score the in-scope notes against `query` and return the best `k`.
/// Split out from `retrieve` so the scoring and scoping rules are
/// unit-testable without reading the on-disk note file.
fn rank_notes(notes: &[MemoryNote], query: &str, folder: &str, k: usize) -> Vec<MemoryNote> {
    let q: std::collections::HashSet<String> = tokens(query);

    let mut scored: Vec<(f64, MemoryNote)> = notes
        .iter()
        .filter(|n| note_in_scope(&n.folder, folder))
        .map(|n| {
            let n_tokens: std::collections::HashSet<String> = tokens(&n.text);
            let overlap = overlap_coeff(&q, &n_tokens);
            let base = if n.folder == folder { 0.5 } else { 0.0 };
            let fresh = recency_boost(n.updated_at_ms);
            (overlap + base + fresh, n.clone())
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .filter(|(score, _)| *score > 0.25)
        .take(k)
        .map(|(_, n)| n)
        .collect()
}

/// Dice coefficient between two token sets (0 = disjoint, 1 = identical).
fn overlap_coeff(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    2.0 * inter / (a.len() as f64 + b.len() as f64)
}

/// Small boost for notes touched within the last day.
fn recency_boost(updated_at_ms: i64) -> f64 {
    let age_ms = Utc::now().timestamp_millis() - updated_at_ms;
    if age_ms < 86_400_000 {
        0.1
    } else {
        0.0
    }
}

/// Normalize text into lowercase alphanumeric tokens.
fn tokens(text: &str) -> std::collections::HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 2)
        .map(|t| t.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(text: &str, folder: &str) -> MemoryNote {
        MemoryNote {
            id: "test".to_string(),
            text: text.to_string(),
            folder: folder.to_string(),
            created_at_ms: 0,
            // Fresh, so the recency boost can't be blamed for a leak.
            updated_at_ms: Utc::now().timestamp_millis(),
        }
    }

    #[test]
    fn note_scope_covers_the_folder_and_its_descendants_only() {
        let here = "/home/u/proj";
        assert!(note_in_scope(here, here), "the folder itself");
        assert!(note_in_scope("/home/u", here), "an ancestor covers its descendants");
        assert!(note_in_scope("/", here), "the root is global");
        assert!(!note_in_scope("/home/u/other", here), "a sibling project must not leak");
        assert!(!note_in_scope("/home/u/proj/sub", here), "a descendant note is not a parent fact");
        assert!(!note_in_scope("", here), "unscoped notes are not injected");
    }

    #[test]
    fn note_from_another_project_is_never_injected() {
        // Regression, from a real session: this note was saved while working in
        // ~/devops/DownloadLogs and mentions another project's path. The user
        // then asked about an EMPTY, unrelated workspace and the model went off
        // to read /home/jandel/devops/my-github/test1 — because this note was
        // injected into the system prompt as a fact to trust.
        let foreign = note(
            "can you check this folder as well /home/jandel/devops/my-github/test1 as reference and let know what this project is.",
            "/home/jandel/devops/DownloadLogs",
        );
        let query = "in this workspace can you generate Dockerfile for the Playwright \
                     container that can execute QA tests. Use headless chromium and other \
                     needed packages for this project.";
        let hits = rank_notes(
            &[foreign],
            query,
            "/home/jandel/devops/my-github/docker-playwright",
            5,
        );
        assert!(
            hits.is_empty(),
            "a note from a sibling project leaked into the prompt: {hits:?}"
        );
    }

    #[test]
    fn keyword_overlap_alone_would_have_admitted_it() {
        // Guards the reason the folder gate must be a hard filter, not a boost:
        // generic English scores ~0.26, over the 0.25 relevance threshold.
        let q = tokens(
            "in this workspace can you generate Dockerfile for the Playwright container \
             that can execute QA tests. Use headless chromium and other needed packages for this project.",
        );
        let n = tokens(
            "can you check this folder as well /home/jandel/devops/my-github/test1 as \
             reference and let know what this project is.",
        );
        assert!(
            overlap_coeff(&q, &n) > 0.25,
            "expected the raw keyword score to clear the threshold (got {})",
            overlap_coeff(&q, &n)
        );
    }

    #[test]
    fn local_notes_rank_above_ancestor_notes() {
        let folder = "/home/u/proj";
        let local = note("this project uses playwright for QA tests", folder);
        let parent = note("we always run playwright headless in CI", "/home/u");
        let hits = rank_notes(&[parent, local], "playwright QA tests", folder, 5);

        assert_eq!(hits.len(), 2, "both the local and the ancestor note are eligible");
        assert_eq!(hits[0].folder, folder, "the local note outranks the ancestor note");

        // Documented behaviour: a note saved in the CURRENT folder is always
        // injected, because the folder boost (0.5) alone already clears the
        // relevance threshold. That is the deliberate "local context wins"
        // design, not an oversight — but it does mean local notes need no
        // keyword overlap at all to reach the prompt.
        let local_but_irrelevant = note("the database runs on postgres", folder);
        let hits = rank_notes(&[local_but_irrelevant], "playwright QA tests", folder, 5);
        assert_eq!(hits.len(), 1, "local notes are injected regardless of overlap");
    }
}