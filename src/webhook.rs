use crate::models::{WaMessage, WebhookPayload};
use crate::AppState;
use actix_web::{web, HttpRequest, HttpResponse, Responder};
use graflog::app_log;
use serde::Deserialize;

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

pub async fn incoming(
    state: web::Data<AppState>,
    path: web::Path<String>,
    _req: HttpRequest,
    payload: web::Json<WebhookPayload>,
) -> impl Responder {
    // Respond 200 immediately — Meta retries if we're slow
    let tenant_id = path.into_inner();
    let state = state.into_inner();
    let payload = payload.into_inner();

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
) -> anyhow::Result<()> {
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
                if let Err(e) =
                    handle_message(&state, &tenant_id, &phone_number_id, msg).await
                {
                    app_log!(error, error = %e, "Failed to handle WA message");
                }
            }
        }
    }
    Ok(())
}

async fn handle_message(
    state: &AppState,
    tenant_id: &str,
    phone_number_id: &str,
    msg: WaMessage,
) -> anyhow::Result<()> {
    // Only handle text messages for now
    let text = match (msg.msg_type.as_str(), msg.text) {
        ("text", Some(t)) => t.body,
        _ => {
            app_log!(info, msg_type = %msg.msg_type, "Ignoring non-text WA message");
            return Ok(());
        }
    };

    let customer_phone = &msg.from;
    app_log!(
        info,
        tenant_id = %tenant_id,
        customer = %customer_phone,
        "WA message received"
    );

    // Load channel config (wa_token + system_prompt)
    let channel = match state.store.get_channel(phone_number_id).await? {
        Some(c) => c,
        None => {
            app_log!(warn, phone_number_id = %phone_number_id, "No channel config found");
            return Ok(());
        }
    };

    // Mark message as read (shows the customer we received it)
    state
        .wa
        .mark_read(phone_number_id, &channel.wa_token, &msg.id)
        .await;

    // Load conversation history
    let history = state.store.get_session(tenant_id, customer_phone).await?;

    // Load tenant's MCP tools
    let tools = state.store.get_tools(tenant_id).await?;

    // Load tenant's downstream auth
    let auth = state.store.get_downstream_auth(tenant_id).await;

    // Run Claude
    let system = if channel.system_prompt.is_empty() {
        "You are a helpful assistant. Use the available tools to answer the user's request."
            .to_string()
    } else {
        channel.system_prompt.clone()
    };

    let (reply, updated_history) = state
        .claude
        .run(&system, history, &text, &tools, &auth)
        .await?;

    // Send reply to customer
    state
        .wa
        .send_text(phone_number_id, &channel.wa_token, customer_phone, &reply)
        .await?;

    // Save updated conversation history
    state
        .store
        .save_session(tenant_id, customer_phone, &updated_history)
        .await?;

    app_log!(
        info,
        tenant_id = %tenant_id,
        customer = %customer_phone,
        "WA reply sent"
    );

    Ok(())
}
