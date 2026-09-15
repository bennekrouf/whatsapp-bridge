// src/telegram_webhook.rs
//
// Telegram adapter. Telegram POSTs every update to a URL we registered, carrying
// the secret we gave it in a header. Parse the update into (who, what), run the
// turn, send the reply. That is the whole adapter — everything else is shared.
//
//   POST /telegram/webhook/{bot_id}
//
// `bot_id` is the number before the colon in the bot token, and is public; the
// secret in `X-Telegram-Bot-Api-Secret-Token` is what proves the caller is
// Telegram and not someone who guessed the URL.

use crate::error::BridgeError;
use crate::turn::{run_turn, Inbound};
use crate::AppState;
use actix_web::{web, HttpRequest, HttpResponse};
use graflog::app_log;
use serde::Deserialize;

pub const CHANNEL: &str = "telegram";

#[derive(Debug, Deserialize)]
pub struct Update {
    pub message: Option<Message>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub text: Option<String>,
    pub from: Option<User>,
    pub chat: Chat,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
}

/// Turn "/start ABC234" — what a deep link produces — into "ABC234", so the
/// link flow needs no special case downstream. Plain "/start" stays as-is and
/// gets the link instructions like any other unlinked message.
fn unwrap_start_payload(text: &str) -> &str {
    let t = text.trim();
    match t.strip_prefix("/start") {
        Some(rest) if !rest.trim().is_empty() => rest.trim(),
        _ => t,
    }
}

pub async fn incoming(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<Update>,
) -> HttpResponse {
    let bot_id = path.into_inner();

    // Telegram is told 200 for everything it sends, even what we ignore:
    // anything else makes it retry the same update indefinitely.
    let ok = || HttpResponse::Ok().finish();

    let channel = match state.store.get_messaging_channel(CHANNEL, &bot_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            app_log!(warn, bot_id = %bot_id, "Telegram update for an unknown bot");
            return ok();
        }
        Err(e) => {
            app_log!(error, error = %e, "Could not resolve Telegram channel");
            return ok();
        }
    };

    // The secret is the whole authentication of this endpoint.
    let presented = req
        .headers()
        .get("X-Telegram-Bot-Api-Secret-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if presented != channel.webhook_secret {
        app_log!(warn, bot_id = %bot_id, "Telegram update with a wrong secret — ignored");
        return ok();
    }

    let (text, from_id, chat_id) = match &body.message {
        Some(Message { text: Some(t), from: Some(u), chat }) => (t.clone(), u.id, chat.id),
        _ => return ok(), // edits, stickers, joins — not conversation
    };

    let text = unwrap_start_payload(&text).to_string();
    let external_id = from_id.to_string();

    app_log!(info, tenant_id = %channel.tenant_id, "Telegram message received");

    let reply = match run_turn(
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
            app_log!(error, error = %e, "Failed to handle Telegram message");
            let error_type = match &e {
                BridgeError::CircuitOpen => "CircuitOpen",
                BridgeError::ToolLoopExhausted { .. } => "ToolLoopExhausted",
                BridgeError::Gateway(_) | BridgeError::GatewayNetwork(_) => "Gateway",
                BridgeError::Store { .. } | BridgeError::StoreNetwork(_) => "Store",
                _ => "Other",
            };
            state
                .store
                .log_failed_message(CHANNEL, &channel.tenant_id, &external_id, &text, error_type, &e.to_string())
                .await;
            "Something went wrong on my side. Please try again in a moment.".to_string()
        }
    };

    if let Err(e) = state.telegram.send_text(&channel.credential, chat_id, &reply).await {
        app_log!(error, error = %e, "Failed to send Telegram reply");
    }

    ok()
}

#[cfg(test)]
mod tests {
    use super::unwrap_start_payload;

    #[test]
    fn a_deep_link_start_yields_its_payload() {
        assert_eq!(unwrap_start_payload("/start ABC234"), "ABC234");
        assert_eq!(unwrap_start_payload("  /start   kl9x2p "), "kl9x2p");
    }

    #[test]
    fn a_bare_start_and_ordinary_text_pass_through() {
        assert_eq!(unwrap_start_payload("/start"), "/start");
        assert_eq!(unwrap_start_payload("list my tasks"), "list my tasks");
    }
}
