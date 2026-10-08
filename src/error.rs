// src/error.rs
//
// Structured error types for the WhatsApp bridge.
// Replaces generic `anyhow::Error` / `Box<dyn Error>` with specific variants
// so callers can match on error kind for logging, retries, and metrics.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BridgeError {
    // ── AI provider (DeepSeek, Mistral) errors ───────────────────────────────
    #[error("LLM API error (HTTP {status}): {body}")]
    LlmApi { status: u16, body: String },

    #[error("LLM API unreachable: {0}")]
    LlmNetwork(#[source] reqwest::Error),

    #[error("LLM response parse error: {0}")]
    LlmParse(#[source] reqwest::Error),

    #[error("LLM tool loop exceeded {max_rounds} rounds")]
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
    #[error("LLM circuit breaker is open — service temporarily unavailable")]
    CircuitOpen,

    // ── Generic ──────────────────────────────────────────────────────────────
    #[error("{0}")]
    Other(String),
}

impl BridgeError {
    /// The category recorded with a failed message, and shown on the
    /// workspace's connector status. Provider errors are split by cause: "out
    /// of credits" and "service busy" need different people to act.
    ///
    /// The "Claude*" names predate the switch to DeepSeek and Mistral. They are
    /// kept: they are stored with failed messages and matched by the gateway's
    /// connector test, and a rename would orphan both.
    pub fn kind(&self) -> &'static str {
        match self {
            BridgeError::LlmApi { status, body } => provider_kind(*status, body),
            BridgeError::LlmNetwork(_) => "ClaudeNetwork",
            BridgeError::LlmParse(_) => "ClaudeParse",
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
                "The assistant is unavailable right now. The team running it has been told why. Please try again later."
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

/// Classify a provider's error response.
fn provider_kind(status: u16, body: &str) -> &'static str {
    match status {
        // DeepSeek answers 402 "Insufficient Balance"; Mistral may answer 400
        // or 403 naming the exhausted credit or billing.
        402 => "ClaudeCredits",
        400 | 403 if ["credit", "balance", "billing"].iter().any(|w| body.to_lowercase().contains(w)) => {
            "ClaudeCredits"
        }
        401 | 403 => "ClaudeAuth",
        429 | 503 | 529 => "ClaudeOverloaded",
        _ => "ClaudeApi",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn llm(status: u16, body: &str) -> BridgeError {
        BridgeError::LlmApi { status, body: body.to_string() }
    }

    #[test]
    fn provider_errors_are_split_by_who_can_fix_them() {
        let credits = llm(400, r#"{"message":"Your credit balance is too low."}"#);
        assert_eq!(credits.kind(), "ClaudeCredits");
        assert!(credits.reply().contains("unavailable right now"));

        assert_eq!(llm(402, r#"{"error":{"message":"Insufficient Balance"}}"#).kind(), "ClaudeCredits");
        assert_eq!(llm(401, "Authentication Fails").kind(), "ClaudeAuth");
        assert_eq!(llm(403, "forbidden").kind(), "ClaudeAuth");
        assert_eq!(llm(429, "rate limited").kind(), "ClaudeOverloaded");
        assert!(llm(503, "overloaded").reply().contains("try again in a minute"));

        // A 400 for any other reason is not a billing problem.
        assert_eq!(llm(400, "messages: field required").kind(), "ClaudeApi");
    }

    #[test]
    fn no_reply_leaks_the_error() {
        let e = llm(400, "Your credit balance is too low (org org_123, key sk-xyz)");
        // Nor the platform's name: the bot is the customer's, under their brand.
        for leak in ["credit", "org_123", "sk-xyz", "DeepSeek", "Mistral", "api0"] {
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
