use crate::error::{BridgeError, BridgeResult};
use crate::models::{ChannelInfo, ChatMessage, MessagingChannel, ResolvedIdentity, TurnUsage};
use graflog::app_log;

pub struct StoreClient {
    base_url: String,
    secret: String,
    client: reqwest::Client,
}

impl StoreClient {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            secret: std::env::var("API0_INTERNAL_SECRET").unwrap_or_default(),
            client: reqwest::Client::new(),
        }
    }

    /// Whether the store answers its health check — the bridge can do nothing
    /// without it, so a bridge that is up but cut off from the store is down.
    pub async fn ping(&self) -> Result<(), String> {
        let resp = self
            .client
            .get(format!("{}/api/health", self.base_url))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| format!("could not reach the store: {}", e))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("the store answered {}", resp.status().as_u16()))
        }
    }

    // Look up channel config by phone_number_id
    /// A workspace's name and the reference its linking page uses
    /// (`/link/<link_ref>`). `None` when it cannot be read — the caller then
    /// falls back to generic instructions rather than failing the reply.
    pub async fn link_info(&self, tenant_id: &str) -> Option<(String, String)> {
        let resp = self
            .client
            .get(format!("{}/api/internal/link-info/{}", self.base_url, urlencoding::encode(tenant_id)))
            .header("X-Internal-Secret", &self.secret)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: serde_json::Value = resp.json().await.ok()?;
        Some((body["name"].as_str()?.to_string(), body["link_ref"].as_str()?.to_string()))
    }

    /// The AI provider, model and key the assistant answers with: for
    /// `tenant_id`, its region's when the workspace chose one, else the super
    /// admin's choice. `Ok(None)` when neither is configured — the bridge then
    /// uses its own built-in DeepSeek key.
    pub async fn assistant_config(&self, tenant_id: Option<&str>) -> Result<Option<crate::llm::LlmSettings>, String> {
        let mut url = format!("{}/api/internal/assistant-config", self.base_url);
        if let Some(t) = tenant_id {
            url.push_str(&format!("?tenant_id={}", urlencoding::encode(t)));
        }
        let resp = self
            .client
            .get(url)
            .header("X-Internal-Secret", &self.secret)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| format!("could not reach the store: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("the store answered {}", resp.status().as_u16()));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        if !body["configured"].as_bool().unwrap_or(false) {
            return Ok(None);
        }
        let field = |k: &str| body[k].as_str().map(str::to_string).ok_or_else(|| format!("missing {}", k));
        Ok(Some(crate::llm::LlmSettings {
            provider: field("provider")?,
            base_url: field("base_url")?,
            // A store from before regions sends none: every provider it could
            // choose answered here.
            chat_path: field("chat_path").unwrap_or_else(|_| "/v1/chat/completions".into()),
            api_key: field("api_key")?,
            model: field("model")?,
            region: field("region").ok(),
        }))
    }

    pub async fn get_channel(&self, phone_number_id: &str) -> BridgeResult<Option<ChannelInfo>> {
        let url = format!(
            "{}/api/internal/whatsapp/channel/{}",
            self.base_url,
            urlencoding::encode(phone_number_id)
        );
        let resp = self
            .client
            .get(&url)
            .header("X-Internal-Secret", &self.secret)
            .send()
            .await
            .map_err(BridgeError::StoreNetwork)?;

        if resp.status() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(BridgeError::Store {
                status: resp.status().as_u16(),
                message: resp.text().await.unwrap_or_default(),
            });
        }
        Ok(Some(resp.json().await?))
    }

    // Look up channel config by tenant_id
    pub async fn get_channel_by_tenant(&self, tenant_id: &str) -> BridgeResult<Option<serde_json::Value>> {
        let url = format!(
            "{}/api/internal/whatsapp/channel/by-tenant/{}",
            self.base_url,
            urlencoding::encode(tenant_id)
        );
        let resp = self
            .client
            .get(&url)
            .header("X-Internal-Secret", &self.secret)
            .send()
            .await
            .map_err(BridgeError::StoreNetwork)?;

        if resp.status() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(BridgeError::Store {
                status: resp.status().as_u16(),
                message: resp.text().await.unwrap_or_default(),
            });
        }
        Ok(Some(resp.json().await?))
    }

    // Load conversation history
    pub async fn get_session(
        &self,
        tenant_id: &str,
        customer_phone: &str,
    ) -> BridgeResult<Vec<ChatMessage>> {
        let url = format!(
            "{}/api/internal/whatsapp/session/{}/{}",
            self.base_url,
            urlencoding::encode(tenant_id),
            urlencoding::encode(customer_phone)
        );
        let resp = self
            .client
            .get(&url)
            .header("X-Internal-Secret", &self.secret)
            .send()
            .await
            .map_err(BridgeError::StoreNetwork)?;

        if !resp.status().is_success() {
            app_log!(warn, "Failed to load session, starting fresh");
            return Ok(vec![]);
        }

        let body: serde_json::Value = resp.json().await?;
        let history = body
            .get("history")
            .and_then(|h| serde_json::from_value(h.clone()).ok())
            .unwrap_or_default();
        Ok(history)
    }

    // Save conversation history
    pub async fn save_session(
        &self,
        tenant_id: &str,
        customer_phone: &str,
        history: &[ChatMessage],
    ) -> BridgeResult<()> {
        let url = format!(
            "{}/api/internal/whatsapp/session/{}/{}",
            self.base_url,
            urlencoding::encode(tenant_id),
            urlencoding::encode(customer_phone)
        );
        self.client
            .put(&url)
            .header("X-Internal-Secret", &self.secret)
            .json(&serde_json::json!({ "history": history }))
            .send()
            .await
            .map_err(BridgeError::StoreNetwork)?;
        Ok(())
    }

    // Delete stale sessions older than `days`
    pub async fn cleanup_stale_sessions(&self, days: i64) -> BridgeResult<u64> {
        let url = format!(
            "{}/api/internal/whatsapp/sessions/stale?days={}",
            self.base_url, days
        );
        let resp = self
            .client
            .delete(&url)
            .header("X-Internal-Secret", &self.secret)
            .send()
            .await
            .map_err(BridgeError::StoreNetwork)?;

        if !resp.status().is_success() {
            return Err(BridgeError::Store {
                status: resp.status().as_u16(),
                message: "Stale session cleanup failed".to_string(),
            });
        }
        let body: serde_json::Value = resp.json().await?;
        Ok(body.get("deleted").and_then(|v| v.as_u64()).unwrap_or(0))
    }

    // Get tenant downstream auth
    // ── Messaging channels (generic) ─────────────────────────────────────────

    /// A tenant's bot on a platform, by what the platform sends us to route on.
    pub async fn get_messaging_channel(
        &self,
        channel: &str,
        channel_ref: &str,
    ) -> BridgeResult<Option<MessagingChannel>> {
        let url = format!(
            "{}/api/internal/messaging-channels/{}/{}",
            self.base_url,
            urlencoding::encode(channel),
            urlencoding::encode(channel_ref),
        );
        let resp = self
            .client
            .get(&url)
            .header("X-Internal-Secret", &self.secret)
            .send()
            .await
            .map_err(BridgeError::StoreNetwork)?;

        match resp.status().as_u16() {
            200 => {
                let b: serde_json::Value = resp.json().await?;
                Ok(Some(MessagingChannel {
                    tenant_id: b["tenant_id"].as_str().unwrap_or_default().to_string(),
                    credential: b["credential"].as_str().unwrap_or_default().to_string(),
                    webhook_secret: b["webhook_secret"].as_str().unwrap_or_default().to_string(),
                    system_prompt: b["system_prompt"].as_str().unwrap_or_default().to_string(),
                }))
            }
            404 => Ok(None),
            status => Err(BridgeError::Store { status, message: "channel lookup failed".into() }),
        }
    }

    // ── Identity ─────────────────────────────────────────────────────────────

    /// The person behind a messaging identity in this tenant, with the api0 key
    /// minted for them. `None` means nobody has linked this identity yet.
    pub async fn resolve_identity(
        &self,
        channel: &str,
        external_id: &str,
        tenant_id: &str,
    ) -> BridgeResult<Option<ResolvedIdentity>> {
        let url = format!(
            "{}/api/internal/channel-identities/resolve?channel={}&external_id={}&tenant_id={}",
            self.base_url,
            urlencoding::encode(channel),
            urlencoding::encode(external_id),
            urlencoding::encode(tenant_id),
        );
        let resp = self
            .client
            .get(&url)
            .header("X-Internal-Secret", &self.secret)
            .send()
            .await
            .map_err(BridgeError::StoreNetwork)?;

        match resp.status().as_u16() {
            200 => {
                let body: serde_json::Value = resp.json().await?;
                Ok(Some(ResolvedIdentity {
                    user_email: body["user_email"].as_str().unwrap_or_default().to_string(),
                    api_key: body["api_key"].as_str().unwrap_or_default().to_string(),
                    api_key_id: body["api_key_id"].as_str().unwrap_or_default().to_string(),
                }))
            }
            404 => Ok(None),
            status => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                Err(BridgeError::Store {
                    status,
                    message: body["error"].as_str().unwrap_or("identity lookup failed").to_string(),
                })
            }
        }
    }

    /// Redeem a link code sent from a messaging identity. On success, returns
    /// the email it is now linked to.
    pub async fn redeem_link_code(
        &self,
        channel: &str,
        external_id: &str,
        tenant_id: &str,
        code: &str,
    ) -> BridgeResult<String> {
        let url = format!("{}/api/internal/channel-identities/redeem", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("X-Internal-Secret", &self.secret)
            .json(&serde_json::json!({
                "channel": channel,
                "external_id": external_id,
                "tenant_id": tenant_id,
                "code": code,
            }))
            .send()
            .await
            .map_err(BridgeError::StoreNetwork)?;

        let status = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        if status == 200 {
            Ok(body["identity"]["user_email"].as_str().unwrap_or_default().to_string())
        } else {
            // The store's message is written for the person — "that code has
            // expired" — so it is handed straight back to them.
            Err(BridgeError::Store {
                status,
                message: body["error"].as_str().unwrap_or("could not link").to_string(),
            })
        }
    }

    /// Record that a platform delivered a message for this tenant's channel.
    /// Best effort with a short timeout: a failure here must never cost the
    /// person their reply.
    pub async fn record_inbound(&self, channel: &str, tenant_id: &str, linked: Option<bool>) {
        let url = format!("{}/api/internal/channel-inbound", self.base_url);
        let result = self
            .client
            .post(&url)
            .header("X-Internal-Secret", &self.secret)
            .timeout(std::time::Duration::from_secs(2))
            .json(&serde_json::json!({
                "channel": channel,
                "tenant_id": tenant_id,
                "linked": linked,
            }))
            .send()
            .await;
        match result {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => app_log!(warn, status = %resp.status(), "Could not record inbound delivery"),
            Err(e) => app_log!(warn, error = %e, "Could not record inbound delivery (network)"),
        }
    }

    /// Put a turn's tokens on the usage ledger, against the linked person's key
    /// and the tenant whose bot answered — the store deducts the credits.
    /// Best effort with a short timeout: a failure here must never cost the
    /// person their reply.
    pub async fn log_usage(
        &self,
        channel: &str,
        tenant_id: &str,
        identity: &ResolvedIdentity,
        usage: &TurnUsage,
        response_time_ms: i64,
        failed: bool,
    ) {
        if usage.calls == 0 {
            return;
        }
        let url = format!("{}/api/user/usage/log", self.base_url);
        let result = self
            .client
            .post(&url)
            .header("X-Internal-Secret", &self.secret)
            .timeout(std::time::Duration::from_secs(2))
            .json(&usage_log_body(channel, tenant_id, identity, usage, response_time_ms, failed))
            .send()
            .await;
        match result {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => app_log!(warn, status = %resp.status(), tokens = usage.total_tokens, "Could not log assistant usage"),
            Err(e) => app_log!(warn, error = %e, tokens = usage.total_tokens, "Could not log assistant usage (network)"),
        }
    }

    /// Log a failed message to the dead-letter queue.
    /// Fire-and-forget: errors are logged but not propagated.
    pub async fn log_failed_message(
        &self,
        channel: &str,
        tenant_id: &str,
        customer_phone: &str,
        message_text: &str,
        error_type: &str,
        error_detail: &str,
    ) {
        let url = format!("{}/api/internal/whatsapp/failed-messages", self.base_url);
        let body = serde_json::json!({
            "channel": channel,
            "tenant_id": tenant_id,
            "customer_phone": customer_phone,
            "message_text": message_text,
            "error_type": error_type,
            "error_detail": error_detail,
        });

        match self
            .client
            .post(&url)
            .header("X-Internal-Secret", &self.secret)
            .json(&body)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                app_log!(warn, status = %resp.status(), "Failed to log dead letter to store");
            }
            Err(e) => {
                app_log!(warn, error = %e, "Failed to log dead letter to store (network)");
            }
        }
    }
}

fn usage_log_body(
    channel: &str,
    tenant_id: &str,
    identity: &ResolvedIdentity,
    usage: &TurnUsage,
    response_time_ms: i64,
    failed: bool,
) -> serde_json::Value {
    serde_json::json!({
        "key_id": identity.api_key_id,
        "email": identity.user_email,
        "endpoint_path": format!("assistant/{}", channel),
        "method": "LLM",
        "status_code": if failed { 500 } else { 200 },
        "response_time_ms": response_time_ms,
        "tenant_id": tenant_id,
        "usage": {
            "estimated": false,
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens,
            "model": usage.model,
        },
        "metadata": {
            "channel": channel,
            "provider": usage.provider,
            "region": usage.region,
            "llm_calls": usage.calls,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_is_logged_against_the_linked_key_and_the_bots_tenant() {
        let id = ResolvedIdentity { user_email: "a@b.c".into(), api_key: "secret".into(), api_key_id: "key-1".into() };
        let usage = TurnUsage {
            provider: "deepseek".into(),
            model: "deepseek-chat".into(),
            region: Some("china".into()),
            calls: 2,
            input_tokens: 900,
            output_tokens: 100,
            total_tokens: 1000,
        };
        let v = usage_log_body("teams", "t-1", &id, &usage, 1234, false);
        assert_eq!(v["key_id"], "key-1");
        assert_eq!(v["tenant_id"], "t-1");
        assert_eq!(v["usage"]["total_tokens"], 1000);
        assert_eq!(v["endpoint_path"], "assistant/teams");
        assert_eq!(v["metadata"]["region"], "china");
        assert!(!v.to_string().contains("secret"), "the key itself must never be logged");
    }
}
