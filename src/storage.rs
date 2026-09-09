use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use uuid::Uuid;

use crate::config::sessions_dir;

// Public session API. `save_session` and `list_sessions` are used directly by the
// daemon; the helpers below are part of the same API surface and kept for
// callers that want finer-grained session access, so silence dead-code lints
// rather than deleting them.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub working_dir: String,
    pub model: String,
    pub messages: Vec<Message>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub timestamp_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<StoredToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[allow(dead_code)]
#[allow(dead_code)]
pub fn create_session(working_dir: &str, model: &str) -> Result<Session> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp_millis();
    let session = Session {
        id: id.clone(),
        title: format!("session-{}", &id[..8]),
        working_dir: working_dir.to_string(),
        model: model.to_string(),
        messages: Vec::new(),
        created_at_ms: now,
        updated_at_ms: now,
    };
    save_session(&session)?;
    Ok(session)
}

#[allow(dead_code)]
pub fn save_session(session: &Session) -> Result<()> {
    let dir = sessions_dir();
    fs::create_dir_all(&dir).context("creating sessions dir")?;
    let path = dir.join(format!("{}.json", session.id));
    let tmp = dir.join(format!("{}.tmp", session.id));
    let data = serde_json::to_vec_pretty(session)?;
    fs::write(&tmp, data).context("writing session tmp")?;
    fs::rename(&tmp, &path).context("renaming session file")?;
    Ok(())
}

#[allow(dead_code)]
pub fn load_session(id: &str) -> Result<Option<Session>> {
    let path = sessions_dir().join(format!("{}.json", id));
    if !path.exists() {
        return Ok(None);
    }
    let data = fs::read_to_string(&path).context("reading session")?;
    let session: Session = serde_json::from_str(&data).context("parsing session")?;
    Ok(Some(session))
}

#[allow(dead_code)]
pub fn append_message(session_id: &str, role: &str, content: &str) -> Result<()> {
    let mut session = load_session(session_id)?.ok_or_else(|| anyhow::anyhow!("session not found"))?;
    let msg = Message {
        role: role.to_string(),
        content: content.to_string(),
        timestamp_ms: Utc::now().timestamp_millis(),
        tool_calls: None,
        tool_call_id: None,
    };
    session.messages.push(msg);
    session.updated_at_ms = Utc::now().timestamp_millis();
    save_session(&session)?;
    Ok(())
}

pub fn list_sessions() -> Result<Vec<Session>> {
    let dir = sessions_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut sessions = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            let data = fs::read_to_string(&path)?;
            if let Ok(session) = serde_json::from_str::<Session>(&data) {
                sessions.push(session);
            }
        }
    }
    sessions.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
    Ok(sessions)
}
