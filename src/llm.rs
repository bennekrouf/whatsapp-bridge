use crate::circuit_breaker::{CircuitBreaker, CircuitState};
use crate::error::{BridgeError, BridgeResult};
use crate::mcp_client::{McpClient, Tool};
use crate::models::{ChatFunction, ChatMessage, ChatRequest, ChatResponse, ChatTool, TurnUsage};
use graflog::app_log;
use std::sync::Arc;

/// How long a provider choice is trusted before the store is asked again — so
/// a change in the admin page, or a workspace's region, takes effect within
/// this, with no restart.
const SETTINGS_TTL: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_TOOL_ROUNDS: usize = 10;

/// The most of one tool result the model reads in the turn that called it,
/// in characters (about 7k tokens of JSON). An API can answer with hundreds of
/// kilobytes (Azure DevOps' work item types, a long list), and every round of
/// the turn sends it again: unbounded, one such answer made each call cost
/// over 100k tokens.
const MAX_TOOL_RESULT_CHARS: usize = 24_000;

/// What is kept of a tool result once its turn is over. The answer the model
/// wrote from it is in the history; the raw data, sent again with every later
/// message for up to 40 messages, would cost far more than it is worth.
const MAX_SAVED_TOOL_RESULT_CHARS: usize = 2_000;
const MAX_RETRIES: u32 = 3;
// Backoff: 1s, 2s, 4s
const BACKOFF_BASE_MS: u64 = 1000;

/// Which AI provider answers, with what key and model. Every provider here
/// speaks the OpenAI-style chat-completions API, tool calls included, at
/// `{base_url}{chat_path}` — so one client serves them all.
#[derive(Clone, Debug, PartialEq)]
pub struct LlmSettings {
    pub provider: String,
    pub base_url: String,
    /// `/v1/chat/completions` for most; Gemini, Qwen and GLM have their own.
    pub chat_path: String,
    pub api_key: String,
    pub model: String,
    /// The region the workspace chose, when it runs on one.
    pub region: Option<String>,
}

impl LlmSettings {
    fn endpoint(&self, path: &str) -> String {
        // A store from before the switch hands DeepSeek's Anthropic-compatible
        // base URL; the chat-completions API is at the root.
        let base = self.base_url.trim_end_matches('/').trim_end_matches("/anthropic");
        format!("{}{}", base, path)
    }

    fn chat_url(&self) -> String {
        self.endpoint(&self.chat_path)
    }
}

/// What the store says a workspace's assistant answers with.
pub enum AssistantConfig {
    Provider(LlmSettings),
    /// Nothing chosen: the workspace has picked no region.
    NotChosen,
    /// The workspace chose this region and it cannot answer. Nothing else may.
    RegionUnavailable(String),
}

/// Who answers a workspace now, or why nobody may.
#[derive(Clone)]
enum Answerer {
    Use(LlmSettings),
    /// No region chosen. Nothing answers until the workspace picks one.
    NotChosen,
    Refuse(String),
}

impl Answerer {
    fn summary(&self) -> String {
        match self {
            Answerer::Use(s) => format!("{}/{}/{:?}", s.provider, s.model, s.region),
            Answerer::NotChosen => "no region chosen".to_string(),
            Answerer::Refuse(why) => format!("refused: {why}"),
        }
    }
}

/// The settings cache's key for the platform's provider, which only the
/// bridge's status page looks at: no workspace answers with it.
const PLATFORM: &str = "";

pub struct LlmClient {
    /// What the store says each workspace (and the platform, under
    /// [`PLATFORM`]) answers with, and when it was read.
    chosen: tokio::sync::RwLock<std::collections::HashMap<String, (Answerer, std::time::Instant)>>,
    store: crate::store_client::StoreClient,
    max_tokens: u32,
    http: reqwest::Client,
    /// One per provider: workspaces on different regions run on different
    /// providers, and one that is down (say, out of credits) must not stop the
    /// others.
    circuits: std::sync::Mutex<std::collections::HashMap<String, Arc<CircuitBreaker>>>,
}

impl LlmClient {
    pub fn new(max_tokens: u32, store: crate::store_client::StoreClient) -> Self {
        Self {
            chosen: tokio::sync::RwLock::new(std::collections::HashMap::new()),
            store,
            max_tokens,
            http: reqwest::Client::new(),
            circuits: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The platform's provider, if the super admin set one.
    pub async fn settings(&self) -> Option<LlmSettings> {
        match self.answerer(None).await {
            Answerer::Use(s) => Some(s),
            Answerer::NotChosen | Answerer::Refuse(_) => None,
        }
    }

    /// Who `tenant_id`'s assistant answers with now: the region it chose, and
    /// nobody if it chose none; `None` for the platform's. Re-read at most every
    /// [`SETTINGS_TTL`]. A store that cannot be read keeps the last known
    /// answer; with none known nobody answers.
    async fn answerer(&self, tenant_id: Option<&str>) -> Answerer {
        let key = tenant_id.unwrap_or(PLATFORM);
        if let Some((s, at)) = self.chosen.read().await.get(key) {
            if at.elapsed() < SETTINGS_TTL {
                return s.clone();
            }
        }
        let mut cache = self.chosen.write().await;
        let previous = cache.get(key).map(|(s, _)| s.clone());
        let next = match self.store.assistant_config(tenant_id).await {
            Ok(AssistantConfig::Provider(s)) => Answerer::Use(s),
            Ok(AssistantConfig::NotChosen) => Answerer::NotChosen,
            Ok(AssistantConfig::RegionUnavailable(region)) => {
                Answerer::Refuse(format!("region {region} is not on offer"))
            }
            Err(e) => {
                app_log!(warn, error = %e, tenant_id = %key, "Could not read the assistant's provider; keeping the last one");
                match &previous {
                    Some(p) => p.clone(),
                    // Not cached: the next message asks the store again.
                    None => return Answerer::Refuse("the store could not be read".into()),
                }
            }
        };
        if previous.as_ref().is_some_and(|p| p.summary() != next.summary()) {
            app_log!(info, tenant_id = %key, now = %next.summary(), "Assistant provider changed");
        }
        cache.insert(key.to_string(), (next.clone(), std::time::Instant::now()));
        next
    }

    fn circuit(&self, provider: &str) -> Arc<CircuitBreaker> {
        let mut circuits = self.circuits.lock().unwrap_or_else(|e| e.into_inner());
        circuits.entry(provider.to_string()).or_insert_with(|| Arc::new(CircuitBreaker::new())).clone()
    }

    /// The circuit of `provider`'s calls.
    pub fn circuit_state(&self, provider: &str) -> CircuitState {
        self.circuit(provider).state()
    }

    /// Whether the platform's provider accepts its key, without spending a token.
    ///
    /// Deliberately outside the circuit breaker: this is how an operator finds
    /// out *why* the circuit opened, so it must not be refused by it, and a
    /// failed check must not count toward tripping it.
    pub async fn check_key(&self) -> Result<(), String> {
        let Some(s) = self.settings().await else {
            return Ok(());
        };
        if s.api_key.is_empty() {
            return Err(format!("no {} key", s.provider));
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
    ///
    /// Tokens are added to `usage` as each completion returns, so a turn that
    /// fails after some rounds still accounts for what it spent.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        system_prompt: &str,
        history: Vec<ChatMessage>,
        user_message: &str,
        tools: &[Tool],
        mcp: &McpClient,
        api_key: &str,
        tenant_id: &str,
        usage: &mut TurnUsage,
    ) -> BridgeResult<(String, Vec<ChatMessage>)> {
        let chat_tools: Vec<ChatTool> = tools.iter().map(to_chat_tool).collect();
        // One provider for the whole turn, even if the choice changes midway.
        let llm = match self.answerer(Some(tenant_id)).await {
            Answerer::Use(s) => s,
            Answerer::NotChosen => {
                app_log!(warn, tenant_id = %tenant_id, "Assistant refused: no AI region chosen");
                return Err(BridgeError::AiRegionNotChosen);
            }
            Answerer::Refuse(why) => {
                app_log!(warn, tenant_id = %tenant_id, reason = %why, "Assistant refused: its AI region cannot answer");
                return Err(BridgeError::AiRegionUnavailable(why));
            }
        };
        usage.provider = llm.provider.clone();
        usage.model = llm.model.clone();
        usage.region = llm.region.clone();

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
            match &resp.usage {
                Some(u) => usage.add(u),
                None => {
                    app_log!(warn, provider = %llm.provider, "Completion came back without token usage");
                    usage.calls += 1;
                }
            }
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
                shorten_tool_results(&mut messages);
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

                let failed = outcome.is_error;
                // Chat completions have no is_error flag, so a refusal is
                // labelled in the text — the model then works around it rather
                // than treating it as an answer.
                let content = if outcome.is_error {
                    format!("Error: {}", outcome.text)
                } else {
                    outcome.text
                };
                let content = clip(
                    &content,
                    MAX_TOOL_RESULT_CHARS,
                    "Ask the tool for fewer items or a narrower query if more is needed.",
                );
                let message = ChatMessage {
                    tool_call_id: Some(call.id.clone()),
                    name: Some(call.function.name.clone()),
                    ..ChatMessage::text("tool", content)
                };
                (message, failed)
            }))
            .await;
            for (call, (message, failed)) in calls.iter().zip(results) {
                usage.record_tool_call(&call.function.name, failed);
                messages.push(message);
            }
        }

        Err(BridgeError::ToolLoopExhausted { max_rounds: MAX_TOOL_ROUNDS })
    }

    /// Call the provider with exponential backoff retry and circuit breaker.
    async fn call_api_with_retry(&self, llm: &LlmSettings, req: &ChatRequest) -> BridgeResult<ChatResponse> {
        let circuit = self.circuit(&llm.provider);
        if !circuit.allow_request() {
            app_log!(warn, provider = %llm.provider, "LLM circuit breaker is OPEN — rejecting request");
            return Err(BridgeError::CircuitOpen);
        }

        let mut last_err = BridgeError::Other("No attempts made".into());

        for attempt in 0..MAX_RETRIES {
            match self.call_api(llm, req).await {
                Ok(resp) => {
                    circuit.record_success();
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
                        circuit.record_failure();
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

        circuit.record_failure();
        Err(last_err)
    }

    async fn call_api(&self, llm: &LlmSettings, req: &ChatRequest) -> BridgeResult<ChatResponse> {
        let resp = self
            .http
            .post(llm.chat_url())
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

/// `text` cut to its first `max` characters, with a note saying so and what
/// to do, so the model knows the data is incomplete.
fn clip(text: &str, max: usize, advice: &str) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}\n\n[Cut: this result has {total} characters; only the first {max} are shown. {advice}]")
}

/// Tool results from turns that are over, cut to what is worth keeping.
fn shorten_tool_results(messages: &mut [ChatMessage]) {
    for m in messages.iter_mut().filter(|m| m.role == "tool") {
        if let Some(text) = m.content.as_str() {
            if text.chars().count() > MAX_SAVED_TOOL_RESULT_CHARS {
                let short = clip(text, MAX_SAVED_TOOL_RESULT_CHARS, "Call the tool again for the full data.");
                m.content = serde_json::Value::String(short);
            }
        }
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
/// - Tool results from earlier turns are cut short (shorten_tool_results),
///   including those saved before that rule.
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
    shorten_tool_results(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_result_is_cut_with_a_note_and_a_short_one_is_not() {
        assert_eq!(clip("short", 10, "x"), "short");
        let long = "é".repeat(30);
        let cut = clip(&long, 10, "Ask for less.");
        assert!(cut.starts_with(&"é".repeat(10)), "cut on characters, not bytes");
        assert!(cut.contains("has 30 characters; only the first 10 are shown. Ask for less."), "{cut}");
    }

    #[test]
    fn earlier_turns_keep_only_the_head_of_their_tool_results() {
        let big = "x".repeat(MAX_SAVED_TOOL_RESULT_CHARS * 50);
        let h = normalize_history(vec![
            ChatMessage::text("user", "list types"),
            ChatMessage { tool_calls: Some(vec![call("a")]), ..ChatMessage::text("assistant", "") },
            ChatMessage { tool_call_id: Some("a".into()), ..ChatMessage::text("tool", big) },
            ChatMessage::text("assistant", "There are 12 types."),
        ]);
        let tool = h.iter().find(|m| m.role == "tool").expect("kept");
        let len = tool.content.as_str().unwrap().chars().count();
        assert!(len < MAX_SAVED_TOOL_RESULT_CHARS + 200, "{len}");
        assert_eq!(h.last().unwrap().content.as_str(), Some("There are 12 types."));
    }
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
    fn token_usage_adds_up_across_rounds_and_may_be_missing() {
        let with: ChatResponse = serde_json::from_value(serde_json::json!({
            "choices": [],
            "usage": { "prompt_tokens": 120, "completion_tokens": 30 }
        }))
        .unwrap();
        let without: ChatResponse = serde_json::from_value(serde_json::json!({ "choices": [] })).unwrap();
        assert!(without.usage.is_none());

        let mut turn = TurnUsage::default();
        turn.add(with.usage.as_ref().unwrap());
        turn.add(&crate::models::ChatUsage { prompt_tokens: 200, completion_tokens: 10, total_tokens: 210 });
        assert_eq!((turn.calls, turn.input_tokens, turn.output_tokens, turn.total_tokens), (2, 320, 40, 360));
    }

    /// A store that answers every request with `body`.
    async fn store_answering(body: &'static str) -> crate::store_client::StoreClient {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        crate::store_client::StoreClient::new(base)
    }

    fn client(store: crate::store_client::StoreClient) -> LlmClient {
        LlmClient::new(100, store)
    }

    #[tokio::test]
    async fn a_region_that_cannot_answer_is_never_replaced() {
        let store = store_answering(r#"{"success":true,"configured":false,"region_unavailable":"europe"}"#).await;
        assert!(matches!(client(store).answerer(Some("t1")).await, Answerer::Refuse(_)));
    }

    #[tokio::test]
    async fn a_workspace_without_a_region_gets_no_provider() {
        let store = store_answering(r#"{"success":true,"configured":false,"region_unavailable":null}"#).await;
        assert!(matches!(client(store).answerer(Some("t1")).await, Answerer::NotChosen));
    }

    #[tokio::test]
    async fn an_unreadable_store_refuses_a_workspace_it_knows_nothing_about() {
        // Nothing listens on this port.
        let store = crate::store_client::StoreClient::new("http://127.0.0.1:9".into());
        let c = client(store);
        assert!(matches!(c.answerer(Some("t1")).await, Answerer::Refuse(_)));
        assert!(c.settings().await.is_none(), "no provider is made up in its place");
    }

    #[test]
    fn the_old_anthropic_base_url_still_reaches_chat_completions() {
        let s = LlmSettings {
            provider: "deepseek".into(),
            base_url: "https://api.deepseek.com/anthropic".into(),
            chat_path: "/v1/chat/completions".into(),
            api_key: String::new(),
            model: "deepseek-chat".into(),
            region: None,
        };
        assert_eq!(s.chat_url(), "https://api.deepseek.com/v1/chat/completions");
    }

    #[test]
    fn a_provider_with_its_own_chat_path_is_called_there() {
        let s = LlmSettings {
            provider: "google".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
            chat_path: "/v1beta/openai/chat/completions".into(),
            api_key: String::new(),
            model: "gemini".into(),
            region: Some("usa".into()),
        };
        assert_eq!(s.chat_url(), "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions");
    }
}
