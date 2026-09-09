use serde::{Deserialize, Serialize};

/// Client -> Server request. Each request carries a unique `id` so responses
/// (including streamed deltas) can be correlated on the client side.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Send a user message to a session. `session_id` is optional: when absent
    /// the server creates a fresh session for this turn (one-shot `run`).
    Message {
        id: u64,
        #[serde(default)]
        session_id: Option<String>,
        content: String,
        #[serde(default)]
        tools: Option<Vec<String>>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    Cancel { id: u64, session_id: Option<String> },
    Ping { id: u64 },
    GetHistory { id: u64, session_id: Option<String> },
    /// Probe configured MCP servers: connect, initialize, and list their tools.
    /// Responded with `Event::McpInfo`. Safe read-only introspection.
    McpProbe { id: u64 },

    // ---- Swarm / multi-agent (jancode-style, in-process) ----
    /// Spawn a new agent session inside the same daemon. The new session runs
    /// `initial_message` headlessly and reports its final response back to
    /// `parent_session_id` as a soft-interrupt notification.
    SwarmSpawn {
        id: u64,
        parent_session_id: Option<String>,
        initial_message: String,
        model: Option<String>,
        label: Option<String>,
    },
    /// Direct message to another session. Delivered as a soft interrupt that
    /// is injected into the target agent's current turn (or queued if idle).
    SwarmDM {
        id: u64,
        from_session_id: Option<String>,
        to_session_id: String,
        message: String,
    },
    /// Broadcast to every session in the same swarm.
    SwarmBroadcast {
        id: u64,
        from_session_id: Option<String>,
        message: String,
    },
    /// Stop a session. `force` is required to stop a session outside the
    /// caller's spawn subtree.
    SwarmStop {
        id: u64,
        session_id: String,
        force: bool,
    },
    /// Snapshot of a single swarm member's live state.
    SwarmStatus { id: u64, session_id: Option<String> },
    /// List every member of the current swarm.
    SwarmList { id: u64 },
}

/// Server -> Client event. Streamed responses are a sequence of `TextDelta`
/// events terminated by a single `Done`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Ack { id: u64 },
    TextDelta { id: Option<u64>, text: String },
    Done { id: Option<u64> },
    Error { id: Option<u64>, message: String },
    History { id: u64, messages: Vec<Message> },
    Pong { id: u64 },
    /// Response to `Request::McpProbe`: per-server connection status and the
    /// tools they expose.
    McpInfo {
        id: u64,
        servers: Vec<crate::mcp::McpServerProbe>,
    },

    // ---- Swarm / multi-agent ----
    /// A new agent session was spawned.
    Spawned { id: u64, new_session_id: String, label: Option<String> },
    /// A soft-interrupt notification (DM, broadcast, or spawn completion report)
    /// addressed to this session. The client should inject it into the agent's
    /// current turn at the next safe point.
    Notification {
        id: Option<u64>,
        from_session: Option<String>,
        notification_type: NotificationType,
        message: String,
    },
    /// Live state snapshot for one swarm member.
    MemberStatus {
        id: u64,
        session_id: String,
        status: String,
        detail: Option<String>,
        task_label: Option<String>,
    },
    /// AI thinking status.
    Status { id: Option<u64>, message: String },
    /// Full member roster for the current swarm.
    MemberList {
        id: u64,
        members: Vec<MemberInfo>,
    },
    /// A session was stopped / removed from the swarm.
    Stopped { id: u64, session_id: String },
    /// The model requested execution of one or more tools. The client may
    /// render these for the user. The server executes them and continues the
    /// turn automatically.
    ToolCall { id: Option<u64>, calls: Vec<ToolCall> },
    /// The result of executing a tool call. The client may render these.
    ToolResult { id: Option<u64>, results: Vec<ToolResultEntry> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NotificationType {
    /// A direct message from another agent.
    Dm,
    /// A broadcast to the whole swarm.
    Broadcast,
    /// A spawn-completion report forwarded from a child agent back to its
    /// parent (jancode's `report_back_to_session_id` policy).
    CompletionReport,
    /// A lifecycle event (started / stopped / crashed).
    Lifecycle,
    /// A session was stopped / removed from the swarm.
    Stopped,
}

/// Public-facing projection of a swarm member, used by `swarm list` /
/// `swarm status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberInfo {
    pub session_id: String,
    pub label: Option<String>,
    pub status: String,
    pub detail: Option<String>,
    pub task_label: Option<String>,
    pub is_headless: bool,
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// A tool call from the model (OpenAI-compatible).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub input: serde_json::Value,
}

/// A tool result entry appended to the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultEntry {
    pub tool_call_id: String,
    pub output: String,
    #[serde(default)]
    pub is_error: bool,
}

/// Generate a request id that is unique within this process. Using the current
/// nanos means each `jancode run` gets a fresh session id (one-shot, no resume),
/// matching jancode's per-invocation session semantics.
pub fn new_message_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}