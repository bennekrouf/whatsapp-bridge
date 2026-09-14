use crate::error::{BridgeError, BridgeResult};
use crate::models::{ChannelInfo, ClaudeMessage, MessagingChannel, ResolvedIdentity};
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

    // Look up channel config by phone_number_id
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
    ) -> BridgeResult<Vec<ClaudeMessage>> {
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
        history: &[ClaudeMessage],
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

    /// Log a failed message to the dead-letter queue.
    /// Fire-and-forget: errors are logged but not propagated.
    pub async fn log_failed_message(
        &self,
        tenant_id: &str,
        customer_phone: &str,
        message_text: &str,
        error_type: &str,
        error_detail: &str,
    ) {
        let url = format!("{}/api/internal/whatsapp/failed-messages", self.base_url);
        let body = serde_json::json!({
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
