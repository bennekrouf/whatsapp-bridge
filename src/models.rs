use serde::{Deserialize, Serialize};

// ── Meta webhook payload ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct WebhookPayload {
    pub object: String,
    pub entry: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
pub struct Entry {
    pub changes: Vec<Change>,
}

#[derive(Debug, Deserialize)]
pub struct Change {
    pub value: ChangeValue,
    pub field: String,
}

#[derive(Debug, Deserialize)]
pub struct ChangeValue {
    pub metadata: Metadata,
    pub messages: Option<Vec<WaMessage>>,
    pub statuses: Option<Vec<serde_json::Value>>, // delivery receipts — ignored
}

#[derive(Debug, Deserialize)]
pub struct Metadata {
    pub phone_number_id: String,
}

#[derive(Debug, Deserialize)]
pub struct WaMessage {
    pub from: String,    // customer phone number
    pub id: String,      // message ID (for dedup)
    #[serde(rename = "type")]
    pub msg_type: String,
    pub text: Option<WaText>,
}

#[derive(Debug, Deserialize)]
pub struct WaText {
    pub body: String,
}

// ── Store channel/session responses ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ChannelInfo {
    pub tenant_id: String,
    pub wa_token: String,
    pub verify_token: String,
    pub system_prompt: String,
}

#[derive(Debug, Deserialize)]
pub struct SessionResponse {
    pub history: Vec<ClaudeMessage>,
}

// ── Claude API ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaudeMessage {
    pub role: String,
    pub content: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct ClaudeRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: String,
    pub tools: Vec<ClaudeTool>,
    pub messages: Vec<ClaudeMessage>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ClaudeTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct ClaudeResponse {
    pub stop_reason: String,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    // text block
    pub text: Option<String>,
    // tool_use block
    pub id: Option<String>,
    pub name: Option<String>,
    pub input: Option<serde_json::Value>,
}

// ── Identity ──────────────────────────────────────────────────────────────────

/// A messaging identity resolved to the api0 person behind it.
#[derive(Debug, Clone)]
pub struct ResolvedIdentity {
    pub user_email: String,
    /// Their own api0 key, pinned to this tenant. Used for every tool call.
    pub api_key: String,
}

/// A tenant's bot on a messaging platform, as the bridge needs it.
#[derive(Debug, Clone)]
pub struct MessagingChannel {
    pub tenant_id: String,
    /// The platform credential — a Telegram bot token, say.
    pub credential: String,
    /// Presented by the platform on every update; proves the caller is them.
    pub webhook_secret: String,
    pub system_prompt: String,
}
