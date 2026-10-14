// src/teams_api.rs
//
// The Bot Connector calls the bridge needs to answer on Teams.
//
// Unlike Telegram, the credential is not used as-is: the bot's App ID and
// secret are exchanged for an access token (valid about an hour, cached here),
// and the reply is POSTed to the `serviceUrl` the inbound activity came from:
//
//   POST {serviceUrl}/v3/conversations/{conversationId}/activities
//
// Only call this with a `serviceUrl` that teams_auth has verified.

use crate::error::{BridgeError, BridgeResult};
use crate::telegram_api::split_message;
use graflog::app_log;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Teams refuses messages over about 28 KB; stay well under it.
const MAX_MESSAGE_CHARS: usize = 20_000;
const SCOPE: &str = "https://api.botframework.com/.default";
/// Multi-tenant bots get their tokens from the Bot Framework's own tenant.
const MULTI_TENANT: &str = "botframework.com";
/// A token is renewed this long before it actually expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(5 * 60);

/// What is sealed in `messaging_channels.credential` for a Teams bot. The App
/// ID itself is the channel_ref.
#[derive(Debug, Deserialize)]
pub struct TeamsCredential {
    pub app_password: String,
    /// The customer's Azure tenant for a single-tenant bot (the default for
    /// new Azure Bots). Empty for a legacy multi-tenant one.
    #[serde(default)]
    pub azure_tenant_id: String,
}

impl TeamsCredential {
    pub fn parse(sealed_json: &str) -> BridgeResult<Self> {
        serde_json::from_str(sealed_json)
            .map_err(|e| BridgeError::Other(format!("unreadable Teams credential: {e}")))
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

pub struct TeamsClient {
    http: reqwest::Client,
    /// App ID → (token, renew after).
    tokens: Mutex<HashMap<String, (String, Instant)>>,
}

/// Where a reply goes: the conversation an activity came from.
pub struct Destination<'a> {
    pub service_url: &'a str,
    pub conversation_id: &'a str,
}

impl TeamsClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            tokens: Mutex::new(HashMap::new()),
        }
    }

    pub async fn send_text(
        &self,
        app_id: &str,
        cred: &TeamsCredential,
        to: &Destination<'_>,
        text: &str,
    ) -> BridgeResult<()> {
        for chunk in split_message(text, MAX_MESSAGE_CHARS) {
            self.send_activity(
                app_id,
                cred,
                to,
                serde_json::json!({ "type": "message", "textFormat": "markdown", "text": chunk }),
            )
            .await?;
        }
        Ok(())
    }

    /// The "…" bubble. Teams shows it for a few seconds; a turn with tool calls
    /// takes longer, so it is sent once as a sign of life, not kept up.
    pub async fn send_typing(&self, app_id: &str, cred: &TeamsCredential, to: &Destination<'_>) -> BridgeResult<()> {
        self.send_activity(app_id, cred, to, serde_json::json!({ "type": "typing" })).await
    }

    async fn send_activity(
        &self,
        app_id: &str,
        cred: &TeamsCredential,
        to: &Destination<'_>,
        activity: serde_json::Value,
    ) -> BridgeResult<()> {
        let token = self.token(app_id, cred).await?;
        let url = activities_url(to.service_url, to.conversation_id);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&activity)
            .send()
            .await
            .map_err(BridgeError::TeamsNetwork)?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let body = resp.text().await.unwrap_or_default();
            if status == 401 || status == 403 {
                // A token the connector refuses will not get better by reuse.
                self.tokens.lock().await.remove(app_id);
            }
            app_log!(error, status = status, body = %body, "Teams send failed");
            return Err(BridgeError::TeamsApi { status, body });
        }
        Ok(())
    }

    async fn token(&self, app_id: &str, cred: &TeamsCredential) -> BridgeResult<String> {
        if let Some((t, renew_after)) = self.tokens.lock().await.get(app_id) {
            if Instant::now() < *renew_after {
                return Ok(t.clone());
            }
        }

        let tenant = match cred.azure_tenant_id.trim() {
            "" => MULTI_TENANT,
            t => t,
        };
        let resp = self
            .http
            .post(format!("https://login.microsoftonline.com/{}/oauth2/v2.0/token", urlencoding::encode(tenant)))
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", app_id),
                ("client_secret", cred.app_password.as_str()),
                ("scope", SCOPE),
            ])
            .send()
            .await
            .map_err(BridgeError::TeamsNetwork)?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let body = resp.text().await.unwrap_or_default();
            app_log!(error, status = status, body = %body, "Teams token request refused");
            return Err(BridgeError::TeamsAuth { status, body });
        }
        let t: TokenResponse = resp.json().await.map_err(BridgeError::TeamsNetwork)?;

        let lifetime = Duration::from_secs(t.expires_in).saturating_sub(EXPIRY_MARGIN);
        self.tokens
            .lock()
            .await
            .insert(app_id.to_string(), (t.access_token.clone(), Instant::now() + lifetime));
        Ok(t.access_token)
    }
}

/// A channel conversation id carries ";messageid=…", so it is encoded whole.
fn activities_url(service_url: &str, conversation_id: &str) -> String {
    format!(
        "{}/v3/conversations/{}/activities",
        service_url.trim_end_matches('/'),
        urlencoding::encode(conversation_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reply_url_joins_cleanly_and_encodes_the_conversation() {
        assert_eq!(
            activities_url("https://smba.trafficmanager.net/emea/", "a:1;messageid=2"),
            "https://smba.trafficmanager.net/emea/v3/conversations/a%3A1%3Bmessageid%3D2/activities"
        );
    }

    #[test]
    fn the_credential_reads_with_or_without_a_tenant() {
        let c = TeamsCredential::parse(r#"{"app_password":"s3cret"}"#).unwrap();
        assert_eq!(c.azure_tenant_id, "");
        let c = TeamsCredential::parse(r#"{"app_password":"s","azure_tenant_id":"t-1"}"#).unwrap();
        assert_eq!(c.azure_tenant_id, "t-1");
        assert!(TeamsCredential::parse("raw-token").is_err());
    }
}
