// src/mcp_client.rs
//
// The bridge as an MCP client of the api0 gateway.
//
// This replaces a hand-rolled tool executor that read tool definitions and
// downstream auth straight from the store and POSTed JSON at backend URLs. That
// worked for one shape of backend and quietly broke for every other: no URL
// placeholders, no content types, no body templates, no per-user credentials.
//
// The gateway already does all of that for Claude. A message from a phone is
// just another MCP client, so it goes through the same door with the same kind
// of key — and inherits every capability the gateway grows, forever, for free.

use crate::error::{BridgeError, BridgeResult};
use graflog::app_log;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// One tool as the gateway advertises it — the shape an LLM tool definition
/// needs, and nothing about how it is executed.
#[derive(Debug, Clone, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default = "empty_schema")]
    pub input_schema: Value,
}

fn empty_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

/// The outcome of a tool call, as the model should see it.
pub struct ToolOutcome {
    pub text: String,
    /// The gateway marks a backend's refusal with isError so the model can read
    /// it and try again, rather than being handed a protocol failure.
    pub is_error: bool,
}

pub struct McpClient {
    endpoint: String,
    http: reqwest::Client,
    next_id: AtomicU64,
}

impl McpClient {
    pub fn new(gateway_url: &str) -> Self {
        Self {
            endpoint: format!("{}/mcp", gateway_url.trim_end_matches('/')),
            http: reqwest::Client::new(),
            next_id: AtomicU64::new(1),
        }
    }

    /// The tools this key may use — the person's tenant, as the gateway sees it.
    pub async fn list_tools(&self, api_key: &str) -> BridgeResult<Vec<Tool>> {
        let result = self.call(api_key, "tools/list", json!({})).await?;
        serde_json::from_value(result["tools"].clone())
            .map_err(|e| BridgeError::Gateway(format!("tools/list returned an unexpected shape: {}", e)))
    }

    pub async fn call_tool(&self, api_key: &str, name: &str, arguments: &Value) -> ToolOutcome {
        match self
            .call(api_key, "tools/call", json!({ "name": name, "arguments": arguments }))
            .await
        {
            Ok(result) => {
                let is_error = result["isError"].as_bool().unwrap_or(false);
                let text = result["content"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b["type"] == "text")
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| result.to_string());
                ToolOutcome { text, is_error }
            }
            // A JSON-RPC error is api0 refusing the call — unknown tool, bad
            // arguments, no credits. Still worth handing to the model as text: it
            // will tell the person, which beats a silent failure.
            Err(e) => ToolOutcome { text: format!("Tool call failed: {}", e), is_error: true },
        }
    }

    async fn call(&self, api_key: &str, method: &str, params: Value) -> BridgeResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let resp = self
            .http
            .post(&self.endpoint)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(BridgeError::GatewayNetwork)?;

        let status = resp.status();
        let payload: Value = resp
            .json()
            .await
            .map_err(|e| BridgeError::Gateway(format!("gateway returned non-JSON ({}): {}", status, e)))?;

        if let Some(err) = payload.get("error") {
            let message = err["message"].as_str().unwrap_or("unknown error");
            app_log!(warn, method = %method, error = %message, "Gateway returned a JSON-RPC error");
            return Err(BridgeError::Gateway(message.to_string()));
        }

        payload
            .get("result")
            .cloned()
            .ok_or_else(|| BridgeError::Gateway("gateway response had neither result nor error".into()))
    }
}
