use crate::models::{ChannelInfo, ClaudeMessage, DownstreamAuth, McpTool};
use anyhow::{anyhow, Result};
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
    pub async fn get_channel(&self, phone_number_id: &str) -> Result<Option<ChannelInfo>> {
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
            .await?;

        if resp.status() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(anyhow!("Store returned {}", resp.status()));
        }
        Ok(Some(resp.json().await?))
    }

    // Look up channel config by tenant_id
    pub async fn get_channel_by_tenant(&self, tenant_id: &str) -> Result<Option<serde_json::Value>> {
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
            .await?;

        if resp.status() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(anyhow!("Store returned {}", resp.status()));
        }
        Ok(Some(resp.json().await?))
    }

    // Load conversation history
    pub async fn get_session(
        &self,
        tenant_id: &str,
        customer_phone: &str,
    ) -> Result<Vec<ClaudeMessage>> {
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
            .await?;

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
    ) -> Result<()> {
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
            .await?;
        Ok(())
    }

    // Get tenant MCP tools
    pub async fn get_tools(&self, tenant_id: &str) -> Result<Vec<McpTool>> {
        let url = format!("{}/api/mcp-tools/{}", self.base_url, urlencoding::encode(tenant_id));
        let resp = self.client.get(&url).send().await?;
        if !resp.status().is_success() {
            return Ok(vec![]);
        }
        let body: serde_json::Value = resp.json().await?;
        let tools = body
            .get("tools")
            .and_then(|t| serde_json::from_value(t.clone()).ok())
            .unwrap_or_default();
        Ok(tools)
    }

    // Get tenant downstream auth
    pub async fn get_downstream_auth(&self, tenant_id: &str) -> DownstreamAuth {
        let url = format!(
            "{}/api/tenant/downstream-auth/{}",
            self.base_url,
            urlencoding::encode(tenant_id)
        );
        match self
            .client
            .get(&url)
            .header("X-Internal-Secret", &self.secret)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = r.json().await.unwrap_or_default();
                serde_json::from_value(body.get("auth").cloned().unwrap_or_default())
                    .unwrap_or_default()
            }
            _ => DownstreamAuth::default(),
        }
    }
}
