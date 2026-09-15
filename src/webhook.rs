use crate::error::{BridgeError, BridgeResult};
use crate::models::{WaMessage, WebhookPayload};
use crate::AppState;
use actix_web::{web, HttpRequest, HttpResponse, Responder};
use crate::turn::{run_turn, Inbound};
use graflog::app_log;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

// ── GET /webhook/{tenant_id}  — Meta hub verification ────────────────────────

#[derive(Deserialize)]
pub struct VerifyQuery {
    #[serde(rename = "hub.mode")]
    pub mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    pub verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    pub challenge: Option<String>,
}

pub async fn verify(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<VerifyQuery>,
) -> impl Responder {
    let tenant_id = path.into_inner();

    let expected = match state
        .store
        .get_channel_by_tenant(&tenant_id)
        .await
        .ok()
        .flatten()
    {
        Some(ch) => ch
            .get("verify_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        None => {
            app_log!(warn, tenant_id = %tenant_id, "Webhook verify: unknown tenant");
            return HttpResponse::Forbidden().body("Unknown tenant");
        }
    };

    if query.mode.as_deref() == Some("subscribe")
        && query.verify_token.as_deref() == Some(&expected)
    {
        let challenge = query.challenge.clone().unwrap_or_default();
        app_log!(info, tenant_id = %tenant_id, "Webhook verified");
        HttpResponse::Ok().body(challenge)
    } else {
        app_log!(warn, tenant_id = %tenant_id, "Webhook verify failed: bad token");
        HttpResponse::Forbidden().body("Verification failed")
    }
}

// ── POST /webhook/{tenant_id}  — incoming messages ───────────────────────────

/// Validate the X-Hub-Signature-256 header against the raw request body.
/// Meta signs every webhook POST with HMAC-SHA256 using the app secret.
/// Returns Ok(()) on valid signature, Err(reason) on failure.
fn validate_signature(secret: &str, signature_header: &str, body: &[u8]) -> Result<(), String> {
    // Header format: "sha256=<hex>"
    let hex_sig = signature_header
        .strip_prefix("sha256=")
        .ok_or_else(|| "Missing sha256= prefix".to_string())?;

    let expected_bytes = hex::decode(hex_sig)
        .map_err(|_| "Invalid hex in signature header".to_string())?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| "Invalid HMAC key".to_string())?;

    mac.update(body);

    mac.verify_slice(&expected_bytes)
        .map_err(|_| "Signature mismatch".to_string())
}

/// How to treat a webhook POST's signature, given what secrets exist.
#[derive(Debug, PartialEq)]
pub enum SignaturePolicy {
    /// Verify against this secret.
    Check(String),
    /// No secret, and the operator explicitly allowed unsigned webhooks.
    AllowUnsigned,
    /// No secret and no override: refuse.
    Refuse,
}

/// The tenant's own App Secret wins — Meta signs with the secret of the app the
/// webhook is configured on, and every tenant brings its own app. The platform's
/// META_APP_SECRET is the fallback. With neither, the answer is no, unless the
/// operator opted out for local development.
pub fn signature_policy(
    tenant_secret: Option<String>,
    platform_secret: Option<String>,
    allow_unsigned: bool,
) -> SignaturePolicy {
    match tenant_secret.or(platform_secret) {
        Some(secret) => SignaturePolicy::Check(secret),
        None if allow_unsigned => SignaturePolicy::AllowUnsigned,
        None => SignaturePolicy::Refuse,
    }
}

pub async fn incoming(
    state: web::Data<AppState>,
    path: web::Path<String>,
    req: HttpRequest,
    body: web::Bytes,
) -> impl Responder {
    let tenant_id = path.into_inner();

    // ── Signature validation ────────────────────────────────────────────────
    // Meta signs with the App Secret of the app the webhook is configured on,
    // and every tenant brings its own app. So the secret is the tenant's first;
    // the platform's META_APP_SECRET only covers a tenant that has not given one
    // (and is right for at most one app). One shared secret for everyone would
    // reject every other tenant's messages with a 401 that Meta retries forever.
    let channel = match state.store.get_channel_by_tenant(&tenant_id).await {
        Ok(Some(ch)) => ch,
        Ok(None) => {
            app_log!(warn, tenant_id = %tenant_id, "Webhook POST for a tenant with no WhatsApp channel");
            return HttpResponse::NotFound().body("Unknown tenant");
        }
        Err(e) => {
            // 5xx on purpose: Meta retries, and the message is not lost to a blip.
            app_log!(error, error = %e, tenant_id = %tenant_id, "Webhook POST: channel lookup failed");
            return HttpResponse::InternalServerError().body("Channel lookup failed");
        }
    };

    let tenant_secret = channel
        .get("app_secret")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(str::to_string);

    match signature_policy(tenant_secret, state.meta_app_secret.clone(), state.allow_unsigned_webhooks) {
        SignaturePolicy::Check(secret) => {
            let sig_header = req
                .headers()
                .get("X-Hub-Signature-256")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");

            if sig_header.is_empty() {
                app_log!(warn, tenant_id = %tenant_id, "Webhook POST missing X-Hub-Signature-256 header");
                return HttpResponse::Unauthorized().body("Missing signature");
            }

            if let Err(reason) = validate_signature(&secret, sig_header, &body) {
                app_log!(warn, tenant_id = %tenant_id, reason = %reason, "Webhook signature validation failed");
                return HttpResponse::Unauthorized().body("Invalid signature");
            }
        }
        SignaturePolicy::AllowUnsigned => {
            app_log!(
                warn,
                tenant_id = %tenant_id,
                "Webhook POST accepted WITHOUT a signature check (ALLOW_UNSIGNED_WEBHOOKS is on)"
            );
        }
        // Fail closed. The bridge acts as whoever the payload's sender is linked
        // to, with their own api0 key — so an unchecked POST from anyone who
        // knows the webhook URL and a linked phone number would run tools as
        // that person. No secret means no way to tell Meta from them.
        SignaturePolicy::Refuse => {
            app_log!(
                warn,
                tenant_id = %tenant_id,
                "Webhook POST refused: no App Secret for this tenant and no META_APP_SECRET"
            );
            return HttpResponse::Unauthorized().body("No App Secret configured for this workspace");
        }
    }

    // ── Parse JSON payload ──────────────────────────────────────────────────
    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            app_log!(error, error = %e, "Failed to parse webhook payload");
            return HttpResponse::BadRequest().body("Invalid payload");
        }
    };

    // Respond 200 immediately — Meta retries if we're slow
    let state = state.into_inner();

    tokio::spawn(async move {
        if let Err(e) = process_payload(state, tenant_id, payload).await {
            app_log!(error, error = %e, "Failed to process WA payload");
        }
    });

    HttpResponse::Ok().body("OK")
}

async fn process_payload(
    state: std::sync::Arc<AppState>,
    tenant_id: String,
    payload: WebhookPayload,
) -> BridgeResult<()> {
    if payload.object != "whatsapp_business_account" {
        return Ok(());
    }

    for entry in payload.entry {
        for change in entry.changes {
            if change.field != "messages" {
                continue;
            }
            let phone_number_id = change.value.metadata.phone_number_id.clone();
            let messages = match change.value.messages {
                Some(m) => m,
                None => continue,
            };
            for msg in messages {
                // Capture metadata before handle_message consumes msg
                let customer_phone = msg.from.clone();
                let msg_text_preview = msg.text.as_ref()
                    .map(|t| t.body.chars().take(500).collect::<String>())
                    .unwrap_or_default();

                if let Err(e) =
                    handle_message(&state, &tenant_id, &phone_number_id, msg).await
                {
                    app_log!(error, error = %e, "Failed to handle WA message");

                    // Classify error type for the dead-letter record
                    let error_type = match &e {
                        BridgeError::ClaudeApi { .. } => "ClaudeApi",
                        BridgeError::ClaudeNetwork(_) => "ClaudeNetwork",
                        BridgeError::ClaudeParse(_) => "ClaudeParse",
                        BridgeError::CircuitOpen => "CircuitOpen",
                        BridgeError::ToolLoopExhausted { .. } => "ToolLoopExhausted",
                        BridgeError::Store { .. } => "Store",
                        BridgeError::StoreNetwork(_) => "StoreNetwork",
                        BridgeError::GatewayNetwork(_) => "GatewayNetwork",
                        BridgeError::Gateway(_) => "Gateway",
                        BridgeError::WhatsAppApi { .. } => "WhatsAppApi",
                        BridgeError::WhatsAppNetwork(_) => "WhatsAppNetwork",
                        BridgeError::TelegramApi { .. } => "TelegramApi",
                        BridgeError::TelegramNetwork(_) => "TelegramNetwork",
                        BridgeError::ChannelNotFound(_) => "ChannelNotFound",
                        BridgeError::Other(_) => "Other",
                    };

                    state.store.log_failed_message(
                        CHANNEL,
                        &tenant_id,
                        &customer_phone,
                        &msg_text_preview,
                        error_type,
                        &e.to_string(),
                    ).await;
                }
            }
        }
    }
    Ok(())
}

/// What this bridge is, to the identity store. A Telegram bridge would say
/// "telegram"; nothing else about linking would change.
const CHANNEL: &str = "whatsapp";



async fn handle_message(
    state: &AppState,
    tenant_id: &str,
    phone_number_id: &str,
    msg: WaMessage,
) -> BridgeResult<()> {
    // Only handle text messages for now
    let text = match (msg.msg_type.as_str(), msg.text) {
        ("text", Some(t)) => t.body,
        _ => {
            app_log!(info, msg_type = %msg.msg_type, "Ignoring non-text WA message");
            return Ok(());
        }
    };

    let customer_phone = &msg.from;

    app_log!(info, tenant_id = %tenant_id, customer = %customer_phone, "WA message received");

    // Channel config: token to reply with, prompt to think with.
    let channel = match state.store.get_channel(phone_number_id).await? {
        Some(c) => c,
        None => {
            app_log!(warn, phone_number_id = %phone_number_id, "No channel config found");
            return Ok(());
        }
    };

    // The URL names the tenant and the payload names the number; they must be
    // the same channel. Otherwise a payload signed with one tenant's secret could
    // name another tenant's number, and run a turn in one tenant while replying
    // with the other's token.
    if channel.tenant_id != tenant_id {
        app_log!(
            warn,
            url_tenant = %tenant_id,
            number_tenant = %channel.tenant_id,
            "WA payload names a number belonging to a different tenant — ignored"
        );
        return Ok(());
    }

    // Mark message as read (shows the customer we received it)
    state.wa.mark_read(phone_number_id, &channel.wa_token, &msg.id).await;

    // Everything that is not WhatsApp-specific happens in one place.
    let reply = run_turn(
        state,
        Inbound {
            channel: CHANNEL,
            external_id: customer_phone,
            tenant_id,
            system_prompt: &channel.system_prompt,
            text: &text,
        },
    )
    .await?;

    state
        .wa
        .send_text(phone_number_id, &channel.wa_token, customer_phone, &reply)
        .await?;

    app_log!(
        info,
        tenant_id = %tenant_id,
        customer = %customer_phone,
        "WA reply sent"
    );

    Ok(())
}


#[cfg(test)]
mod tests {
    use super::{signature_policy, SignaturePolicy};

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn the_tenants_own_secret_wins_over_the_platforms() {
        assert_eq!(
            signature_policy(some("tenant"), some("platform"), false),
            SignaturePolicy::Check("tenant".into())
        );
    }

    #[test]
    fn the_platform_secret_covers_a_tenant_without_one() {
        assert_eq!(
            signature_policy(None, some("platform"), false),
            SignaturePolicy::Check("platform".into())
        );
    }

    #[test]
    fn no_secret_at_all_is_refused_by_default() {
        assert_eq!(signature_policy(None, None, false), SignaturePolicy::Refuse);
    }

    #[test]
    fn unsigned_is_only_accepted_when_explicitly_allowed() {
        assert_eq!(signature_policy(None, None, true), SignaturePolicy::AllowUnsigned);
        // The override never downgrades a check that a secret makes possible.
        assert_eq!(
            signature_policy(some("tenant"), None, true),
            SignaturePolicy::Check("tenant".into())
        );
    }
}
