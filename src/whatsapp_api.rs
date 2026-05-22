use anyhow::Result;
use graflog::app_log;

pub struct WhatsAppClient {
    client: reqwest::Client,
}

impl WhatsAppClient {
    pub fn new() -> Self {
        Self { client: reqwest::Client::new() }
    }

    pub async fn send_text(
        &self,
        phone_number_id: &str,
        wa_token: &str,
        to: &str,
        text: &str,
    ) -> Result<()> {
        let url = format!(
            "https://graph.facebook.com/v19.0/{}/messages",
            phone_number_id
        );

        let body = serde_json::json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": to,
            "type": "text",
            "text": { "body": text }
        });

        let resp = self
            .client
            .post(&url)
            .bearer_auth(wa_token)
            .json(&body)
            .send()
            .await?;

        if resp.status().is_success() {
            app_log!(info, to = %to, "WA message sent");
        } else {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            app_log!(error, to = %to, status = %status, err = %err, "WA send failed");
        }
        Ok(())
    }

    /// Send a "typing…" indicator so the user knows we're working on it
    pub async fn mark_read(&self, phone_number_id: &str, wa_token: &str, message_id: &str) {
        let url = format!(
            "https://graph.facebook.com/v19.0/{}/messages",
            phone_number_id
        );
        let body = serde_json::json!({
            "messaging_product": "whatsapp",
            "status": "read",
            "message_id": message_id
        });
        let _ = self
            .client
            .post(&url)
            .bearer_auth(wa_token)
            .json(&body)
            .send()
            .await;
    }
}
