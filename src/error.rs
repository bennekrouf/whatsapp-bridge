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
