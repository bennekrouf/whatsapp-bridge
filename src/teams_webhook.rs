// src/teams_webhook.rs
//
// Microsoft Teams adapter. Teams POSTs every activity to the messaging endpoint
// set on the customer's Azure Bot, signed with a Bot Framework JWT.
//
//   POST /teams/webhook/{app_id}
//
// `app_id` is the bot's Microsoft App ID — public, and what the token's
// audience must be. Three things differ from Telegram:
//
//   - Authentication is the JWT (teams_auth), not a shared secret. A request
//     that fails it gets 401: there is no retry storm to avoid, unlike
//     Telegram, because Microsoft's own requests always pass.
//   - Teams wants an answer within ~15 s, and a turn with tool calls can take
//     longer. So the activity is acknowledged at once and the turn runs in the
//     background; the reply goes out as a new activity.
//   - Teams retries what it thinks was lost, so activities are de-duplicated.
//
// Only one-to-one chats are served. In a channel or group chat the session
// would be shared by everyone in it while tools run as one person — that needs
// its own design before it is allowed.

use crate::teams_api::{Destination, TeamsCredential};
use crate::turn::{run_turn, Inbound};
use crate::AppState;
use actix_web::{web, HttpRequest, HttpResponse};
use graflog::app_log;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

pub const CHANNEL: &str = "teams";

const GROUP_REPLY: &str = "I only work in a one-to-one chat. Open a chat with me directly and ask there.";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub id: String,
    pub service_url: String,
    #[serde(default)]
    pub channel_id: String,
    pub from: Option<Account>,
    pub conversation: Conversation,
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    /// The person's Entra ID object id. Stable across bots, unlike `id`.
    pub aad_object_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub id: String,
    #[serde(default)]
    pub conversation_type: String,
}

/// Activities seen recently, so a retried one is not answered twice.
static SEEN: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const SEEN_FOR: Duration = Duration::from_secs(10 * 60);

/// True the first time an activity is seen within `SEEN_FOR`.
fn first_sighting(key: String) -> bool {
    let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    if seen.len() > 10_000 {
        seen.retain(|_, at| now.duration_since(*at) < SEEN_FOR);
    }
    match seen.get(&key) {
        Some(at) if now.duration_since(*at) < SEEN_FOR => false,
        _ => {
            seen.insert(key, now);
            true
        }
    }
}

/// Teams puts "<at>Bot name</at>" in the text wherever someone mentions the
/// bot. That is addressing, not content.
fn strip_mentions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<at>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</at>") {
            Some(end) => rest = &rest[start + end + "</at>".len()..],
            None => {
                rest = &rest[start..];
                break;
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub async fn incoming(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<Activity>,
) -> HttpResponse {
    let app_id = path.into_inner();
    let activity = body.into_inner();

    let channel = match state.store.get_messaging_channel(CHANNEL, &app_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            app_log!(warn, app_id = %app_id, "Teams activity for an unknown bot");
            return HttpResponse::NotFound().finish();
        }
        Err(e) => {
            app_log!(error, error = %e, "Could not resolve Teams channel");
            // 5xx: Teams retries, and the store may be back by then.
            return HttpResponse::ServiceUnavailable().finish();
        }
    };

    let authorization = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if let Err(reason) = state.teams_auth.verify(authorization, &app_id, &activity.service_url).await {
        app_log!(warn, app_id = %app_id, reason = %reason, "Teams activity refused");
        return HttpResponse::Unauthorized().finish();
    }

    let accepted = || HttpResponse::Accepted().finish();

    // Signed for this bot, but by some other Bot Framework channel.
    if activity.channel_id != "msteams" {
        return accepted();
    }
    // Installs, reactions, card clicks — not conversation.
    if activity.kind != "message" {
        return accepted();
    }
    if !activity.id.is_empty() && !first_sighting(format!("{}:{}", app_id, activity.id)) {
        app_log!(info, app_id = %app_id, "Teams activity already handled — ignored");
        return accepted();
    }

    let Some(from) = activity.from else { return accepted() };
    let text = strip_mentions(activity.text.as_deref().unwrap_or_default());
    if text.is_empty() {
        return accepted();
    }

    let cred = match TeamsCredential::parse(&channel.credential) {
        Ok(c) => c,
        Err(e) => {
            app_log!(error, tenant_id = %channel.tenant_id, error = %e, "Teams channel unusable");
            return accepted();
        }
    };

    let external_id = from.aad_object_id.filter(|s| !s.is_empty()).unwrap_or(from.id);
    let personal = activity.conversation.conversation_type == "personal";
    let service_url = activity.service_url;
    let conversation_id = activity.conversation.id;

    app_log!(info, tenant_id = %channel.tenant_id, personal = personal, "Teams message received");

    // Everything below may outlast Teams' patience, so it is not awaited here.
    tokio::spawn(async move {
        let to = Destination { service_url: &service_url, conversation_id: &conversation_id };

        let reply = if !personal {
            GROUP_REPLY.to_string()
        } else {
            let _ = state.teams.send_typing(&app_id, &cred, &to).await;
            match run_turn(
                &state,
                Inbound {
                    channel: CHANNEL,
                    external_id: &external_id,
                    tenant_id: &channel.tenant_id,
                    system_prompt: &channel.system_prompt,
                    text: &text,
                },
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    app_log!(error, error = %e, "Failed to handle Teams message");
                    state
                        .store
                        .log_failed_message(CHANNEL, &channel.tenant_id, &external_id, &text, e.kind(), &e.to_string())
                        .await;
                    e.reply().to_string()
                }
            }
        };

        if let Err(e) = state.teams.send_text(&app_id, &cred, &to, &reply).await {
            app_log!(error, error = %e, "Failed to send Teams reply");
            state
                .store
                .log_failed_message(CHANNEL, &channel.tenant_id, &external_id, &text, e.kind(), &e.to_string())
                .await;
        }
    });

    accepted()
}

#[cfg(test)]
mod tests {
    use super::{first_sighting, strip_mentions, Activity};

    #[test]
    fn mentions_of_the_bot_are_removed() {
        assert_eq!(strip_mentions("<at>Ribh</at> list my tasks"), "list my tasks");
        assert_eq!(strip_mentions("hey <at>Ribh Bot</at>,  what's due?"), "hey , what's due?");
        assert_eq!(strip_mentions("ABC234"), "ABC234");
        assert_eq!(strip_mentions("<at>Ribh</at>"), "");
        // An unclosed tag is left as typed rather than eating the message.
        assert_eq!(strip_mentions("a <at>b"), "a <at>b");
    }

    #[test]
    fn a_retried_activity_is_seen_only_once() {
        assert!(first_sighting("app:act-1".into()));
        assert!(!first_sighting("app:act-1".into()));
        assert!(first_sighting("app:act-2".into()));
    }

    #[test]
    fn a_teams_message_activity_parses() {
        let a: Activity = serde_json::from_str(r#"{
            "type": "message", "id": "1700000000000", "channelId": "msteams",
            "serviceUrl": "https://smba.trafficmanager.net/emea/",
            "from": { "id": "29:1abc", "name": "Sam", "aadObjectId": "00000000-aaaa" },
            "conversation": { "id": "a:1xyz", "conversationType": "personal", "tenantId": "t" },
            "recipient": { "id": "28:app", "name": "Bot" },
            "text": "list my tasks", "textFormat": "plain"
        }"#).unwrap();
        assert_eq!(a.kind, "message");
        assert_eq!(a.conversation.conversation_type, "personal");
        assert_eq!(a.from.unwrap().aad_object_id.as_deref(), Some("00000000-aaaa"));
    }
}
