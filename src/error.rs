// src/error.rs
//
// Structured error types for the WhatsApp bridge.
// Replaces generic `anyhow::Error` / `Box<dyn Error>` with specific variants
// so callers can match on error kind for logging, retries, and metrics.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BridgeError {
    // ── Claude API errors ────────────────────────────────────────────────────
    #[error("Claude API error (HTTP {status}): {body}")]
    ClaudeApi { status: u16, body: String },

    #[error("Claude API unreachable: {0}")]
    ClaudeNetwork(#[source] reqwest::Error),

    #[error("Claude response parse error: {0}")]
    ClaudeParse(#[source] reqwest::Error),

    #[error("Claude tool loop exceeded {max_rounds} rounds")]
    ToolLoopExhausted { max_rounds: usize },

    // ── Store errors ─────────────────────────────────────────────────────────
    #[error("Store returned HTTP {status}: {message}")]
    Store { status: u16, message: String },

    #[error("Store unreachable: {0}")]
    StoreNetwork(#[source] reqwest::Error),

    // ── Gateway (MCP) errors ─────────────────────────────────────────────────
    #[error("Gateway unreachable: {0}")]
    GatewayNetwork(#[source] reqwest::Error),

    #[error("Gateway error: {0}")]
    Gateway(String),

    // ── Meta / WhatsApp errors ───────────────────────────────────────────────
    #[error("WhatsApp API error (HTTP {status}): {body}")]
    WhatsAppApi { status: u16, body: String },

    #[error("WhatsApp API unreachable: {0}")]
    WhatsAppNetwork(#[source] reqwest::Error),

    // ── Telegram errors ──────────────────────────────────────────────────────
    #[error("Telegram API error (HTTP {status}): {body}")]
    TelegramApi { status: u16, body: String },

    #[error("Telegram API unreachable: {0}")]
    TelegramNetwork(#[source] reqwest::Error),

    // ── Channel / config errors ──────────────────────────────────────────────
    #[error("No channel config found for phone_number_id={0}")]
    ChannelNotFound(String),

    // ── Circuit breaker ────────────────────────────────────────────────────
    #[error("Claude circuit breaker is open — service temporarily unavailable")]
    CircuitOpen,

    // ── Generic ──────────────────────────────────────────────────────────────
    #[error("{0}")]
    Other(String),
}

impl BridgeError {
    /// The category recorded with a failed message, and shown on the
    /// workspace's connector status. Claude errors are split by cause: "out of
    /// credits" and "service busy" need different people to act.
    pub fn kind(&self) -> &'static str {
        match self {
            BridgeError::ClaudeApi { status, body } => claude_kind(*status, body),
            BridgeError::ClaudeNetwork(_) => "ClaudeNetwork",
            BridgeError::ClaudeParse(_) => "ClaudeParse",
            BridgeError::CircuitOpen => "CircuitOpen",
            BridgeError::ToolLoopExhausted { .. } => "ToolLoopExhausted",
            BridgeError::Store { .. } => "Store",
            BridgeError::StoreNetwork(_) => "StoreNetwork",
            BridgeError::GatewayNetwork(_) => "GatewayNetwork",
            BridgeError::Gateway(_) => "Gateway",
            BridgeError::WhatsAppApi { .. } => "WhatsAppApi",
            BridgeError::WhatsAppNetwork(_) => "WhatsAppNetwork",
            BridgeError::TelegramApi { .. } => "TelegramApi",
            BridgeError::TelegramNetwork(_) => "TelegramNetwork",
            BridgeError::ChannelNotFound(_) => "ChannelNotFound",
            BridgeError::Other(_) => "Other",
        }
    }

    /// What the person who sent the message is told. Never the error itself:
    /// the bot is shared, and billing, keys and hosts are the operator's
    /// business. But "try again" is only said when trying again can help.
    pub fn reply(&self) -> &'static str {
        match self.kind() {
            // Only the workspace's operator can fix these.
            "ClaudeCredits" | "ClaudeAuth" | "Store" | "StoreNetwork" | "ChannelNotFound" => {
                "The assistant is unavailable right now. The workspace owner can see why in api0. Please try again later."
            }
            // Passing: a retry in a minute will likely work.
            "ClaudeOverloaded" | "ClaudeNetwork" | "CircuitOpen" | "GatewayNetwork" => {
                "I'm overloaded right now. Please try again in a minute."
            }
            "ToolLoopExhausted" => {
                "That needed more steps than I can take in one go. Try asking for something smaller."
            }
            _ => "Something went wrong on my side. Please try again in a moment.",
        }
    }
}

/// Classify an Anthropic API error response.
fn claude_kind(status: u16, body: &str) -> &'static str {
    match status {
        // Anthropic says "credit balance is too low"; DeepSeek answers 402
        // "Insufficient Balance".
        400 if body.to_lowercase().contains("credit balance") => "ClaudeCredits",
        402 => "ClaudeCredits",
        401 | 403 => "ClaudeAuth",
        429 | 503 | 529 => "ClaudeOverloaded",
        _ => "ClaudeApi",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude(status: u16, body: &str) -> BridgeError {
        BridgeError::ClaudeApi { status, body: body.to_string() }
    }

    #[test]
    fn claude_errors_are_split_by_who_can_fix_them() {
        let credits = claude(400, r#"{"error":{"message":"Your credit balance is too low to access the Anthropic API."}}"#);
        assert_eq!(credits.kind(), "ClaudeCredits");
        assert!(credits.reply().contains("workspace owner"));

        assert_eq!(claude(402, r#"{"error":{"message":"Insufficient Balance"}}"#).kind(), "ClaudeCredits");
        assert_eq!(claude(401, "invalid x-api-key").kind(), "ClaudeAuth");
        assert_eq!(claude(529, "overloaded").kind(), "ClaudeOverloaded");
        assert!(claude(529, "overloaded").reply().contains("try again in a minute"));

        // A 400 for any other reason is not a billing problem.
        assert_eq!(claude(400, "messages: field required").kind(), "ClaudeApi");
    }

    #[test]
    fn no_reply_leaks_the_error() {
        let e = claude(400, "Your credit balance is too low (org org_123, key sk-ant-xyz)");
        for leak in ["credit", "org_123", "sk-ant", "Anthropic"] {
            assert!(!e.reply().contains(leak), "reply leaks {}", leak);
        }
    }
}

// Allow `?` on reqwest::Error where we want a generic network bucket
impl From<reqwest::Error> for BridgeError {
    fn from(e: reqwest::Error) -> Self {
        BridgeError::Other(e.to_string())
    }
}

// Allow `?` on serde_json::Error
impl From<serde_json::Error> for BridgeError {
    fn from(e: serde_json::Error) -> Self {
        BridgeError::Other(e.to_string())
    }
}

/// Convenience type alias
pub type BridgeResult<T> = Result<T, BridgeError>;
