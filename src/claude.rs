use crate::circuit_breaker::{CircuitBreaker, CircuitState};
use crate::error::{BridgeError, BridgeResult};
use crate::mcp_client::{McpClient, Tool};
use crate::models::{ClaudeMessage, ClaudeRequest, ClaudeResponse, ClaudeTool, ContentBlock};
use graflog::app_log;
use std::sync::Arc;

const CLAUDE_API_URL: &str = "https://api.anthropic.com/v1/messages";
/// Listing models costs nothing and still authenticates the key.
const CLAUDE_MODELS_URL: &str = "https://api.anthropic.com/v1/models?limit=1";
const MAX_TOOL_ROUNDS: usize = 10;
const MAX_RETRIES: u32 = 3;
// Backoff: 1s, 2s, 4s
const BACKOFF_BASE_MS: u64 = 1000;

pub struct ClaudeClient {
    api_key: String,
    model: String,
    max_tokens: u32,
    http: reqwest::Client,
    circuit: Arc<CircuitBreaker>,
}

impl ClaudeClient {
    pub fn new(api_key: String, model: String, max_tokens: u32) -> Self {
        Self {
            api_key,
            model,
            max_tokens,
            http: reqwest::Client::new(),
            circuit: Arc::new(CircuitBreaker::new()),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn circuit_state(&self) -> CircuitState {
        self.circuit.state()
    }

    /// Whether Anthropic accepts the key, without spending a token.
    ///
    /// Deliberately outside the circuit breaker: this is how an operator finds
    /// out *why* the circuit opened, so it must not be refused by it, and a
    /// failed check must not count toward tripping it.
    pub async fn check_key(&self) -> Result<(), String> {
        let resp = self
            .http
            .get(CLAUDE_MODELS_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| format!("could not reach Anthropic: {}", e))?;

        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        Err(format!(
            "Anthropic answered {}: {}",
            status,
            body["error"]["message"].as_str().unwrap_or("no detail")
        ))
    }

    /// Run a full conversation turn:
    /// - Appends the user message to history
    /// - Loops through tool calls until Claude gives a final text response
    /// - Returns (reply_text, updated_history)
    ///
    /// Every tool call goes to the gateway with `api_key` — the linked person's
    /// own key — so it runs with their credentials and is attributed to them.
    /// This function knows nothing about how a tool is executed, and must not.
    pub async fn run(
        &self,
        system_prompt: &str,
        history: Vec<ClaudeMessage>,
        user_message: &str,
        tools: &[Tool],
        mcp: &McpClient,
        api_key: &str,
    ) -> BridgeResult<(String, Vec<ClaudeMessage>)> {
        let claude_tools: Vec<ClaudeTool> = tools.iter().map(to_claude_tool).collect();

        let mut messages = history;
        messages.push(ClaudeMessage {
            role: "user".into(),
            content: serde_json::json!(user_message),
        });

        for round in 0..MAX_TOOL_ROUNDS {
            let req = ClaudeRequest {
                model: self.model.clone(),
                max_tokens: self.max_tokens,
                system: system_prompt.to_string(),
                tools: claude_tools.clone(),
                messages: messages.clone(),
            };

            let resp = self.call_api_with_retry(&req).await?;

            match resp.stop_reason.as_str() {
                "end_turn" => {
                    let text = extract_text(&resp.content);
                    messages.push(ClaudeMessage {
                        role: "assistant".into(),
                        content: serde_json::json!(content_blocks_to_value(&resp.content)),
                    });
                    return Ok((text, messages));
                }

                "tool_use" => {
                    app_log!(info, round = round, "Claude requested tool calls");
                    // Append assistant turn (with tool_use blocks)
                    messages.push(ClaudeMessage {
                        role: "assistant".into(),
                        content: serde_json::json!(content_blocks_to_value(&resp.content)),
                    });

                    // Execute every tool_use block
                    let mut results = vec![];
                    for block in &resp.content {
                        if block.block_type == "tool_use" {
                            let tool_id = block.id.clone().unwrap_or_default();
                            let tool_name = block.name.clone().unwrap_or_default();
                            let input = block.input.clone().unwrap_or(serde_json::json!({}));

                            let outcome = mcp.call_tool(api_key, &tool_name, &input).await;

                            app_log!(info, tool = %tool_name, is_error = outcome.is_error, "Tool executed via gateway");

                            // Claude's tool_result carries is_error too, so the
                            // model treats a refusal as something to work around
                            // rather than as an answer.
                            results.push(serde_json::json!({
                                "type":        "tool_result",
                                "tool_use_id": tool_id,
                                "content":     outcome.text,
                                "is_error":    outcome.is_error,
                            }));
                        }
                    }

                    messages.push(ClaudeMessage {
                        role: "user".into(),
                        content: serde_json::json!(results),
                    });
                }

                other => {
                    app_log!(warn, stop_reason = %other, "Unexpected stop reason");
                    let text = extract_text(&resp.content);
                    messages.push(ClaudeMessage {
                        role: "assistant".into(),
                        content: serde_json::json!(text),
                    });
                    return Ok((text, messages));
                }
            }
        }

        Err(BridgeError::ToolLoopExhausted { max_rounds: MAX_TOOL_ROUNDS })
    }

    /// Call Claude API with exponential backoff retry and circuit breaker.
    async fn call_api_with_retry(&self, req: &ClaudeRequest) -> BridgeResult<ClaudeResponse> {
        // Circuit breaker check
        if !self.circuit.allow_request() {
            app_log!(warn, "Claude circuit breaker is OPEN — rejecting request");
            return Err(BridgeError::CircuitOpen);
        }

        let mut last_err = BridgeError::Other("No attempts made".into());

        for attempt in 0..MAX_RETRIES {
            match self.call_api(req).await {
                Ok(resp) => {
                    self.circuit.record_success();
                    return Ok(resp);
                }
                Err(e) => {
                    let retryable = matches!(
                        &e,
                        BridgeError::ClaudeNetwork(_)
                            | BridgeError::ClaudeApi { status: 429, .. }
                            | BridgeError::ClaudeApi { status: 500..=599, .. }
                    );

                    if !retryable || attempt == MAX_RETRIES - 1 {
                        // Non-retryable or exhausted retries
                        self.circuit.record_failure();
                        app_log!(
                            error,
                            attempt = attempt + 1,
                            error = %e,
                            "Claude API call failed (not retrying)"
                        );
                        return Err(e);
                    }

                    let backoff_ms = BACKOFF_BASE_MS * 2u64.pow(attempt);
                    app_log!(
                        warn,
                        attempt = attempt + 1,
                        backoff_ms = backoff_ms,
                        error = %e,
                        "Claude API call failed, retrying"
                    );
                    tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                    last_err = e;
                }
            }
        }

        self.circuit.record_failure();
        Err(last_err)
    }

    async fn call_api(&self, req: &ClaudeRequest) -> BridgeResult<ClaudeResponse> {
        let resp = self
            .http
            .post(CLAUDE_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(req)
            .send()
            .await
            .map_err(BridgeError::ClaudeNetwork)?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(BridgeError::ClaudeApi { status, body });
        }
        resp.json().await.map_err(BridgeError::ClaudeParse)
    }
}

// ── Tool execution ────────────────────────────────────────────────────────────

// ── Helpers ───────────────────────────────────────────────────────────────────

fn to_claude_tool(t: &Tool) -> ClaudeTool {
    ClaudeTool {
        name: t.name.clone(),
        description: t.description.clone(),
        input_schema: t.input_schema.clone(),
    }
}

fn extract_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter(|b| b.block_type == "text")
        .filter_map(|b| b.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

fn content_blocks_to_value(blocks: &[ContentBlock]) -> Vec<serde_json::Value> {
    blocks
        .iter()
        .map(|b| serde_json::to_value(b).unwrap_or_default())
        .collect()
}
