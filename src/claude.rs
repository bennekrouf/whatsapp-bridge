use crate::models::{
    ClaudeMessage, ClaudeRequest, ClaudeResponse, ClaudeTool, ContentBlock, DownstreamAuth,
    McpTool,
};
use anyhow::{anyhow, Result};
use graflog::app_log;

const CLAUDE_API_URL: &str = "https://api.anthropic.com/v1/messages";
const MAX_TOOL_ROUNDS: usize = 10;

pub struct ClaudeClient {
    api_key: String,
    model: String,
    max_tokens: u32,
    http: reqwest::Client,
}

impl ClaudeClient {
    pub fn new(api_key: String, model: String, max_tokens: u32) -> Self {
        Self {
            api_key,
            model,
            max_tokens,
            http: reqwest::Client::new(),
        }
    }

    /// Run a full conversation turn:
    /// - Appends the user message to history
    /// - Loops through tool calls until Claude gives a final text response
    /// - Returns (reply_text, updated_history)
    pub async fn run(
        &self,
        system_prompt: &str,
        history: Vec<ClaudeMessage>,
        user_message: &str,
        tools: &[McpTool],
        downstream_auth: &DownstreamAuth,
    ) -> Result<(String, Vec<ClaudeMessage>)> {
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

            let resp = self.call_api(&req).await?;

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

                    // Execute every tool_use block in parallel
                    let mut results = vec![];
                    for block in &resp.content {
                        if block.block_type == "tool_use" {
                            let tool_id = block.id.clone().unwrap_or_default();
                            let tool_name = block.name.clone().unwrap_or_default();
                            let input = block.input.clone().unwrap_or(serde_json::json!({}));

                            let result = execute_tool(
                                tools,
                                &tool_name,
                                &input,
                                downstream_auth,
                                &self.http,
                            )
                            .await;

                            app_log!(info, tool = %tool_name, "Tool executed");

                            results.push(serde_json::json!({
                                "type":        "tool_result",
                                "tool_use_id": tool_id,
                                "content":     result,
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

        Err(anyhow!("Tool call loop exceeded {} rounds", MAX_TOOL_ROUNDS))
    }

    async fn call_api(&self, req: &ClaudeRequest) -> Result<ClaudeResponse> {
        let resp = self
            .http
            .post(CLAUDE_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(req)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Claude API error {}: {}", status, body));
        }
        Ok(resp.json().await?)
    }
}

// ── Tool execution ────────────────────────────────────────────────────────────

async fn execute_tool(
    tools: &[McpTool],
    name: &str,
    input: &serde_json::Value,
    auth: &DownstreamAuth,
    client: &reqwest::Client,
) -> String {
    let tool = match tools.iter().find(|t| t.tool_name == name) {
        Some(t) => t,
        None => return format!("Error: unknown tool '{}'", name),
    };

    let verb = tool.http_verb.as_deref().unwrap_or("POST");
    let timeout = std::time::Duration::from_millis(tool.timeout_ms as u64);

    let mut builder = match verb.to_uppercase().as_str() {
        "GET"    => client.get(&tool.backend_url),
        "DELETE" => client.delete(&tool.backend_url),
        "PUT"    => client.put(&tool.backend_url).json(input),
        "PATCH"  => client.patch(&tool.backend_url).json(input),
        _        => client.post(&tool.backend_url).json(input),
    };

    builder = builder.timeout(timeout);

    // Apply downstream auth
    match auth.auth_mode.as_deref() {
        Some("static_bearer") => {
            if let Some(token) = &auth.bearer_token {
                builder = builder.bearer_auth(token);
            }
        }
        Some("header_injection") => {
            if let Some(headers) = &auth.custom_headers {
                if let Some(obj) = headers.as_object() {
                    for (k, v) in obj {
                        if let Some(v_str) = v.as_str() {
                            builder = builder.header(k.as_str(), v_str);
                        }
                    }
                }
            }
        }
        _ => {}
    }

    match builder.send().await {
        Ok(resp) => resp.text().await.unwrap_or_else(|_| "empty response".into()),
        Err(e)   => format!("Tool call failed: {}", e),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn to_claude_tool(t: &McpTool) -> ClaudeTool {
    let schema = serde_json::from_str(&t.input_schema).unwrap_or_else(|_| {
        serde_json::json!({"type": "object", "properties": {}})
    });
    ClaudeTool {
        name: t.tool_name.clone(),
        description: t.description.clone(),
        input_schema: schema,
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
