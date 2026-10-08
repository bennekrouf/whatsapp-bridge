use crate::circuit_breaker::{CircuitBreaker, CircuitState};
use crate::error::{BridgeError, BridgeResult};
use crate::mcp_client::{McpClient, Tool};
use crate::models::{ChatFunction, ChatMessage, ChatRequest, ChatResponse, ChatTool};
use graflog::app_log;
use std::sync::Arc;

/// The built-in provider, used until the super admin chooses one in api0.
const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com";
/// How long a provider choice is trusted before the store is asked again — so
/// a change in the admin page takes effect within this, with no restart.
const SETTINGS_TTL: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_TOOL_ROUNDS: usize = 10;
const MAX_RETRIES: u32 = 3;
// Backoff: 1s, 2s, 4s
const BACKOFF_BASE_MS: u64 = 1000;

/// Which AI provider answers, with what key and model. Every provider here
/// (DeepSeek, Mistral) speaks the OpenAI-style chat-completions API at
/// `{base_url}/v1/chat/completions`, tool calls included — so one client
/// serves them all.
#[derive(Clone, Debug, PartialEq)]
pub struct LlmSettings {
    pub provider: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl LlmSettings {
    fn endpoint(&self, path: &str) -> String {
        // A store from before the switch hands DeepSeek's Anthropic-compatible
        // base URL; the chat-completions API is at the root.
        let base = self.base_url.trim_end_matches('/').trim_end_matches("/anthropic");
        format!("{}{}", base, path)
    }
}

pub struct LlmClient {
    /// The bridge's own key and model, from its configuration.
    builtin: LlmSettings,
    /// The super admin's choice, read from the store, and when it was read.
    chosen: tokio::sync::RwLock<Option<(LlmSettings, std::time::Instant)>>,
    store: crate::store_client::StoreClient,
    max_tokens: u32,
    http: reqwest::Client,
    circuit: Arc<CircuitBreaker>,
}

impl LlmClient {
    /// `api_key` is the built-in DeepSeek key; empty when the deployment relies
    /// on the provider chosen in the admin page.
    pub fn new(api_key: String, model: String, max_tokens: u32, store: crate::store_client::StoreClient) -> Self {
        Self {
            builtin: LlmSettings {
                provider: "deepseek".into(),
                base_url: DEEPSEEK_BASE_URL.into(),
                api_key,
                model,
            },
            chosen: tokio::sync::RwLock::new(None),
            store,
            max_tokens,
            http: reqwest::Client::new(),
            circuit: Arc::new(CircuitBreaker::new()),
        }
    }

    /// The provider to use now: the super admin's choice when there is one,
    /// re-read at most every [`SETTINGS_TTL`]; the built-in one otherwise. A
    /// store that cannot be read keeps the last known choice.
    pub async fn settings(&self) -> LlmSettings {
        if let Some((s, at)) = self.chosen.read().await.as_ref() {
            if at.elapsed() < SETTINGS_TTL {
                return s.clone();
            }
        }
        let mut slot = self.chosen.write().await;
        let previous = slot.as_ref().map(|(s, _)| s.clone());
        let next = match self.store.assistant_config().await {
            Ok(Some(s)) => s,
            Ok(None) => self.builtin.clone(),
            Err(e) => {
                app_log!(warn, error = %e, "Could not read the assistant's provider; keeping the last one");
                previous.clone().unwrap_or_else(|| self.builtin.clone())
            }
        };
        if previous.as_ref().is_some_and(|p| p.provider != next.provider || p.api_key != next.api_key) {
            // A breaker opened by the old provider (say, out of credits) must
            // not keep the new one from being tried.
            self.circuit.record_success();
            app_log!(info, provider = %next.provider, model = %next.model, "Assistant provider changed");
        }
        *slot = Some((next.clone(), std::time::Instant::now()));
        next
    }

    pub fn circuit_state(&self) -> CircuitState {
        self.circuit.state()
    }

    /// Whether the current provider accepts its key, without spending a token.
    ///
    /// Deliberately outside the circuit breaker: this is how an operator finds
    /// out *why* the circuit opened, so it must not be refused by it, and a
    /// failed check must not count toward tripping it.
    pub async fn check_key(&self) -> Result<(), String> {
        let s = self.settings().await;
        if s.api_key.is_empty() {
            return Err(format!(
                "no {} key — set DEEPSEEK_API_KEY or choose a provider in Admin → Messaging assistant",
                s.provider
            ));
        }
        let url = match s.provider.as_str() {
            // DeepSeek's balance endpoint is free and also says whether there
            // is credit left — the failure that matters most.
            "deepseek" => s.endpoint("/user/balance"),
            _ => s.endpoint("/v1/models"),
        };
        let resp = self
            .http
            .get(url)
            .bearer_auth(&s.api_key)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| format!("could not reach {}: {}", s.provider, e))?;

        let status = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        if (200..300).contains(&status) {
            if s.provider == "deepseek" && body["is_available"].as_bool() == Some(false) {
                return Err("DeepSeek answered, but the account has no balance left".into());
            }
            return Ok(());
        }
        Err(format!(
            "{} answered {}: {}",
            s.provider,
            status,
            body["error"]["message"]
                .as_str()
                .or_else(|| body["message"].as_str())
                .or_else(|| body["detail"].as_str())
                .unwrap_or("no detail")
        ))
    }

    /// Run a full conversation turn:
    /// - Appends the user message to history
    /// - Loops through tool calls until the model gives a final text response
    /// - Returns (reply_text, updated_history)
    ///
    /// Every tool call goes to the gateway with `api_key` — the linked person's
    /// own key — so it runs with their credentials and is attributed to them.
    /// This function knows nothing about how a tool is executed, and must not.
    pub async fn run(
        &self,
        system_prompt: &str,
        history: Vec<ChatMessage>,
        user_message: &str,
        tools: &[Tool],
        mcp: &McpClient,
        api_key: &str,
    ) -> BridgeResult<(String, Vec<ChatMessage>)> {
        let chat_tools: Vec<ChatTool> = tools.iter().map(to_chat_tool).collect();
        // One provider for the whole turn, even if the choice changes midway.
        let llm = self.settings().await;

        let mut messages = normalize_history(history);
        messages.push(ChatMessage::text("user", user_message));

        for round in 0..MAX_TOOL_ROUNDS {
            // The system prompt is not part of the saved history: it can change
            // between turns, and the latest one should apply.
            let mut request_messages = Vec::with_capacity(messages.len() + 1);
            request_messages.push(ChatMessage::text("system", system_prompt));
            request_messages.extend(messages.iter().cloned());
            let req = ChatRequest {
                model: llm.model.clone(),
                max_tokens: self.max_tokens,
                messages: request_messages,
                tools: chat_tools.clone(),
            };

            let resp = self.call_api_with_retry(&llm, &req).await?;
            let Some(choice) = resp.choices.into_iter().next() else {
                return Err(BridgeError::Other(format!("{} answered with no choices", llm.provider)));
            };
            let mut reply = choice.message;
            reply.role = "assistant".into();

            let calls = reply.tool_calls.clone().filter(|c| !c.is_empty());
            let Some(calls) = calls else {
                if choice.finish_reason.as_deref().is_some_and(|r| r != "stop") {
                    app_log!(warn, finish_reason = ?choice.finish_reason, "Unexpected finish reason");
                }
                let text = reply.content.as_str().unwrap_or_default().to_string();
                messages.push(ChatMessage::text("assistant", text.clone()));
                strip_reasoning(&mut messages);
                return Ok((text, messages));
            };

            app_log!(info, round = round, "Model requested tool calls");
            messages.push(reply);

            // Execute every tool call at once: the model asked for them
            // together, so none depends on another's result, and a turn waits
            // for the slowest instead of the sum. join_all keeps the order the
            // model asked in, and every call gets its own `tool` message.
            let results = futures_util::future::join_all(calls.iter().map(|call| async move {
                let input = parse_arguments(&call.function.arguments);
                let outcome = mcp.call_tool(api_key, &call.function.name, &input).await;

                app_log!(info, tool = %call.function.name, is_error = outcome.is_error, "Tool executed via gateway");

                // Chat completions have no is_error flag, so a refusal is
                // labelled in the text — the model then works around it rather
                // than treating it as an answer.
                let content = if outcome.is_error {
                    format!("Error: {}", outcome.text)
                } else {
                    outcome.text
                };
                ChatMessage {
                    tool_call_id: Some(call.id.clone()),
                    name: Some(call.function.name.clone()),
                    ..ChatMessage::text("tool", content)
                }
            }))
            .await;
            messages.extend(results);
        }

        Err(BridgeError::ToolLoopExhausted { max_rounds: MAX_TOOL_ROUNDS })
    }

    /// Call the provider with exponential backoff retry and circuit breaker.
    async fn call_api_with_retry(&self, llm: &LlmSettings, req: &ChatRequest) -> BridgeResult<ChatResponse> {
        // Circuit breaker check
        if !self.circuit.allow_request() {
            app_log!(warn, "LLM circuit breaker is OPEN — rejecting request");
            return Err(BridgeError::CircuitOpen);
        }

        let mut last_err = BridgeError::Other("No attempts made".into());

        for attempt in 0..MAX_RETRIES {
            match self.call_api(llm, req).await {
                Ok(resp) => {
                    self.circuit.record_success();
                    return Ok(resp);
                }
                Err(e) => {
                    let retryable = matches!(
                        &e,
                        BridgeError::LlmNetwork(_)
                            | BridgeError::LlmApi { status: 429, .. }
                            | BridgeError::LlmApi { status: 500..=599, .. }
                    );

                    if !retryable || attempt == MAX_RETRIES - 1 {
                        // Non-retryable or exhausted retries
                        self.circuit.record_failure();
                        app_log!(
                            error,
                            attempt = attempt + 1,
                            provider = %llm.provider,
                            error = %e,
                            "LLM API call failed (not retrying)"
                        );
                        return Err(e);
                    }

                    let backoff_ms = BACKOFF_BASE_MS * 2u64.pow(attempt);
                    app_log!(
                        warn,
                        attempt = attempt + 1,
                        provider = %llm.provider,
                        backoff_ms = backoff_ms,
                        error = %e,
                        "LLM API call failed, retrying"
                    );
                    tokio::time::sleep(tokio::time::Duration::from_millis(backoff_ms)).await;
                    last_err = e;
                }
            }
        }

        self.circuit.record_failure();
        Err(last_err)
    }

    async fn call_api(&self, llm: &LlmSettings, req: &ChatRequest) -> BridgeResult<ChatResponse> {
        let resp = self
            .http
            .post(llm.endpoint("/v1/chat/completions"))
            .bearer_auth(&llm.api_key)
            .json(req)
            .send()
            .await
            .map_err(BridgeError::LlmNetwork)?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(BridgeError::LlmApi { status, body });
        }
        resp.json().await.map_err(BridgeError::LlmParse)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn to_chat_tool(t: &Tool) -> ChatTool {
    ChatTool {
        tool_type: "function",
        function: ChatFunction {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.input_schema.clone(),
        },
    }
}

/// Arguments arrive as a JSON string; an empty or malformed one becomes `{}`
/// and the gateway's own validation says what is missing.
fn parse_arguments(raw: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) if v.is_object() => v,
        _ => serde_json::json!({}),
    }
}

/// DeepSeek's reasoning is only wanted back within the turn that produced it.
fn strip_reasoning(messages: &mut [ChatMessage]) {
    for m in messages {
        m.reasoning_content = None;
    }
}

/// Make a saved history safe to send.
///
/// - A history saved while the bridge spoke Anthropic's API has arrays of
///   content blocks: only their text is kept, and tool_use / tool_result
///   blocks, which have no equivalent without their ids, are dropped.
/// - The store keeps the last N messages, so a history can start mid tool
///   exchange: anything before the first user message is dropped.
/// - `tool` messages whose call is not in the assistant message before them
///   are dropped; providers reject them.
/// - Consecutive plain-text messages of the same role, which the above can
///   leave behind, are merged.
pub fn normalize_history(history: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::with_capacity(history.len());
    let mut open_calls: Vec<String> = Vec::new();

    for mut m in history {
        if !matches!(m.role.as_str(), "user" | "assistant" | "tool") {
            continue;
        }
        if let serde_json::Value::Array(blocks) = &m.content {
            let text = blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() || m.role == "tool" {
                continue;
            }
            m = ChatMessage::text(&m.role, text);
        }
        if out.is_empty() && m.role != "user" {
            continue;
        }
        match m.role.as_str() {
            "tool" => {
                let Some(pos) = m.tool_call_id.as_ref().and_then(|id| open_calls.iter().position(|c| c == id)) else {
                    continue;
                };
                open_calls.remove(pos);
            }
            _ => {
                if !open_calls.is_empty() {
                    // An assistant's tool calls must each be answered before
                    // anything else; a cut-off exchange is dropped whole.
                    while out.last().is_some_and(|l| l.role == "tool" || l.tool_calls.is_some()) {
                        out.pop();
                    }
                    open_calls.clear();
                }
                open_calls = m.tool_calls.iter().flatten().map(|c| c.id.clone()).collect();
                if m.tool_calls.as_ref().is_some_and(|c| c.is_empty()) {
                    m.tool_calls = None;
                }
            }
        }
        m.reasoning_content = None;

        let mergeable = |x: &ChatMessage| x.role != "tool" && x.tool_calls.is_none() && x.content.is_string();
        if let Some(last) = out.last_mut() {
            if last.role == m.role && mergeable(last) && mergeable(&m) {
                let joined = format!("{}\n{}", last.content.as_str().unwrap_or_default(), m.content.as_str().unwrap_or_default());
                last.content = serde_json::Value::String(joined);
                continue;
            }
        }
        out.push(m);
    }
    if !open_calls.is_empty() {
        while out.last().is_some_and(|l| l.role == "tool" || l.tool_calls.is_some()) {
            out.pop();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{FunctionCall, ToolCall};

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall { name: "list_items".into(), arguments: "{}".into() },
        }
    }

    fn tool_reply(id: &str) -> ChatMessage {
        ChatMessage { tool_call_id: Some(id.into()), name: Some("list_items".into()), ..ChatMessage::text("tool", "[]") }
    }

    #[test]
    fn legacy_anthropic_history_keeps_only_its_text() {
        let legacy: Vec<ChatMessage> = serde_json::from_value(serde_json::json!([
            { "role": "user", "content": "list my items" },
            { "role": "assistant", "content": [
                { "type": "text", "text": "Let me check." },
                { "type": "tool_use", "id": "toolu_1", "name": "list_items", "input": {} }
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "toolu_1", "content": "[]", "is_error": false }
            ]},
            { "role": "assistant", "content": [{ "type": "text", "text": "You have none." }] }
        ]))
        .unwrap();

        let h = normalize_history(legacy);
        let shape: Vec<(&str, &str)> = h.iter().map(|m| (m.role.as_str(), m.content.as_str().unwrap())).collect();
        assert_eq!(shape, vec![("user", "list my items"), ("assistant", "Let me check.\nYou have none.")]);
    }

    #[test]
    fn a_history_cut_mid_exchange_starts_at_a_user_message() {
        let h = normalize_history(vec![
            tool_reply("a"),
            ChatMessage::text("assistant", "done"),
            ChatMessage::text("user", "hi"),
            ChatMessage::text("assistant", "hello"),
        ]);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].role, "user");
    }

    #[test]
    fn complete_tool_exchanges_survive_and_unanswered_ones_do_not() {
        let asking = |ids: &[&str]| ChatMessage {
            tool_calls: Some(ids.iter().map(|i| call(i)).collect()),
            ..ChatMessage::text("assistant", "")
        };
        let h = normalize_history(vec![
            ChatMessage::text("user", "one"),
            asking(&["a"]),
            tool_reply("a"),
            ChatMessage::text("assistant", "answer"),
            ChatMessage::text("user", "two"),
            asking(&["b", "c"]),
            tool_reply("b"),
        ]);
        let roles: Vec<&str> = h.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "tool", "assistant", "user"]);
    }

    #[test]
    fn requests_carry_tools_in_the_function_shape() {
        let req = ChatRequest {
            model: "deepseek-chat".into(),
            max_tokens: 10,
            messages: vec![ChatMessage::text("user", "hi")],
            tools: vec![to_chat_tool(&Tool {
                name: "list_items".into(),
                description: "Lists items".into(),
                input_schema: serde_json::json!({ "type": "object", "properties": {} }),
            })],
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["parameters"]["type"], "object");
        assert!(v["messages"][0].get("tool_calls").is_none());
    }

    #[test]
    fn the_old_anthropic_base_url_still_reaches_chat_completions() {
        let s = LlmSettings {
            provider: "deepseek".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            api_key: String::new(),
            model: "deepseek-chat".into(),
        };
        assert_eq!(s.endpoint("/v1/chat/completions"), "https://api.deepseek.com/v1/chat/completions");
    }
}
