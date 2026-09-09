//! In-process multi-agent (swarm) coordination, mirroring jancode's design.
//!
//! jancode's swarm is NOT process-level cloning: every agent session lives inside
//! the single server daemon, keyed by `session_id`. "Spawning" an agent means
//! the server creates a new persisted `Session` + `SwarmMember`, runs an
//! `initial_message` headlessly through its own provider client, and forwards
//! the final response back to the parent as a soft-interrupt notification.
//! Inter-agent DMs route through the server and are injected as soft interrupts
//! into the target agent's current turn.

use std::collections::HashMap;
use tracing::warn;

use crate::protocol::{Event, MemberInfo, NotificationType};

/// A member of a swarm. All members share one daemon process; the only thing
/// that distinguishes them is their persisted session id and their
/// `parent_session_id` parent pointer.
#[derive(Debug, Clone)]
pub struct SwarmMember {
    pub session_id: String,
    pub label: Option<String>,
    pub status: String,
    pub detail: Option<String>,
    pub task_label: Option<String>,
    pub is_headless: bool,
    pub parent_session_id: Option<String>,
    /// Live turn-context buffer for soft-interrupt notifications addressed to
    /// this member while it is mid-turn.
    pub interrupt_queue: Vec<Interrupt>,
}

#[derive(Debug, Clone)]
pub struct Interrupt {
    pub from_session: Option<String>,
    pub notification_type: NotificationType,
    pub message: String,
}

/// Swarm state, held by the server. Each swarm is keyed by a `swarm_id`
/// (here we use a single implicit swarm per daemon, matching jancode's
/// single-server model).
#[derive(Debug, Clone)]
pub struct SwarmState {
    pub swarm_id: String,
    pub members: HashMap<String, SwarmMember>,
    /// session_id -> interrupt queue (kept in sync with `members`).
    pub interrupt_queues: HashMap<String, Vec<Interrupt>>,
}

impl SwarmState {
    pub fn new(swarm_id: String) -> Self {
        Self {
            swarm_id,
            members: HashMap::new(),
            interrupt_queues: HashMap::new(),
        }
    }

    pub fn member_list(&self) -> Vec<MemberInfo> {
        self.members
            .values()
            .map(|m| MemberInfo {
                session_id: m.session_id.clone(),
                label: m.label.clone(),
                status: m.status.clone(),
                detail: m.detail.clone(),
                task_label: m.task_label.clone(),
                is_headless: m.is_headless,
                parent_session_id: m.parent_session_id.clone(),
            })
            .collect()
    }

    pub fn register_member(&mut self, member: SwarmMember) {
        let sid = member.session_id.clone();
        self.interrupt_queues.entry(sid.clone()).or_default();
        self.members.insert(sid, member);
    }

    pub fn remove_member(&mut self, session_id: &str) {
        self.members.remove(session_id);
        self.interrupt_queues.remove(session_id);
    }

    pub fn get_member(&self, session_id: &str) -> Option<&SwarmMember> {
        self.members.get(session_id)
    }

    pub fn get_member_mut(&mut self, session_id: &str) -> Option<&mut SwarmMember> {
        self.members.get_mut(session_id)
    }

    pub fn update_status(&mut self, session_id: &str, status: &str, detail: Option<String>) {
        if let Some(m) = self.members.get_mut(session_id) {
            m.status = status.to_string();
            m.detail = detail;
        }
    }

    /// Push a soft-interrupt notification addressed to `session_id`. If the
    /// session is not a known member, the notification is dropped (mirrors
    /// jancode's behavior of only delivering to swarm members).
    pub fn queue_interrupt(&mut self, session_id: &str, interrupt: Interrupt) {
        if !self.members.contains_key(session_id) {
            warn!(
                "interrupt for unknown member {} dropped",
                session_id
            );
            return;
        }
        self.interrupt_queues
            .entry(session_id.to_string())
            .or_default()
            .push(interrupt);
    }

    /// Drain all pending interrupts for a session, returning them in order.
    pub fn drain_interrupts(&mut self, session_id: &str) -> Vec<Interrupt> {
        self.interrupt_queues
            .remove(session_id)
            .unwrap_or_default()
    }
}

/// Create a new swarm member for a freshly-spawned session.
pub fn make_member(
    session_id: &str,
    label: Option<String>,
    parent_session_id: Option<String>,
    is_headless: bool,
) -> SwarmMember {
    SwarmMember {
        session_id: session_id.to_string(),
        label,
        status: "spawned".to_string(),
        detail: None,
        task_label: None,
        is_headless,
        parent_session_id,
        interrupt_queue: Vec::new(),
    }
}

/// Build the notification event for a DM/broadcast/completion report.
pub fn interrupt_event(
    from_session: Option<&str>,
    notification_type: NotificationType,
    message: String,
) -> Event {
    Event::Notification {
        id: None,
        from_session: from_session.map(String::from),
        notification_type,
        message,
    }
}
