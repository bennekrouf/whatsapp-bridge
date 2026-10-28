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
    // Delivery receipts (`statuses`) arrive here too; with no field for them,
    // serde skips them.
    pub messages: Option<Vec<WaMessage>>,
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
    /// The tenant's Meta App Secret, if they gave one. Absent from a store that
    /// predates it, hence the default.
    #[serde(default)]
    pub app_secret: Option<String>,
}

// ── Chat completions (DeepSeek, Mistral) ──────────────────────────────────────

/// One message of a conversation, in the OpenAI-style chat-completions shape
/// both providers speak. `content` is a Value because a history saved before
/// the switch away from Anthropic holds arrays of content blocks there — see
/// `llm::normalize_history`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The tool's name on a `tool` message; Mistral expects it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// DeepSeek's thinking output. Sent back within a turn's tool rounds,
    /// stripped before the history is saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl ChatMessage {
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: serde_json::Value::String(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub call_type: String,
    pub function: FunctionCall,
}

fn function_type() -> String {
    "function".into()
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded arguments, as a string.
    #[serde(default)]
    pub arguments: String,
}

#[derive(Debug, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ChatTool>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChatTool {
    #[serde(rename = "type")]
    pub tool_type: &'static str,
    pub function: ChatFunction,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChatFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<ChatChoice>,
    /// What the call cost. Every OpenAI-style provider sends it; a missing one
    /// counts as nothing rather than failing the reply.
    #[serde(default)]
    pub usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize, Default, Clone, Copy)]
pub struct ChatUsage {
    #[serde(default)]
    pub prompt_tokens: i64,
    #[serde(default)]
    pub completion_tokens: i64,
    #[serde(default)]
    pub total_tokens: i64,
}

/// Tokens spent on one turn, across every round of tool calls — what is billed
/// to the person the turn ran as.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TurnUsage {
    pub provider: String,
    pub model: String,
    /// The region the workspace chose, when the turn ran on one.
    pub region: Option<String>,
    /// How many completions the turn took.
    pub calls: u32,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

impl TurnUsage {
    pub fn add(&mut self, u: &ChatUsage) {
        self.calls += 1;
        self.input_tokens += u.prompt_tokens;
        self.output_tokens += u.completion_tokens;
        // Some providers leave the total out; it is never less than the parts.
        self.total_tokens += u.total_tokens.max(u.prompt_tokens + u.completion_tokens);
    }
}

#[derive(Debug, Deserialize)]
pub struct ChatChoice {
    pub message: ChatMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

// ── Identity ──────────────────────────────────────────────────────────────────

/// A messaging identity resolved to the api0 person behind it.
#[derive(Debug, Clone)]
pub struct ResolvedIdentity {
    pub user_email: String,
    /// Their own api0 key, pinned to this tenant. Used for every tool call.
    pub api_key: String,
    /// That key's id — what the usage ledger records the turn against.
    pub api_key_id: String,
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
