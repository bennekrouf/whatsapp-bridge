// src/status.rs
//
//   GET /internal/status   (X-Internal-Secret)
//
// `/health` only proves the process answers. The failures that have actually
// taken the bridge down are all behind that: a running binary that predates a
// route, a Claude key that was rotated, a circuit left open after an Anthropic
// outage, a store the bridge can no longer reach. Each is reported here on its
// own so the admin panel can say which.
//
// Behind the internal secret, unlike `/health`: whether webhook signatures are
// being checked is not something to tell the internet.

use crate::circuit_breaker::CircuitState;
use crate::AppState;
use actix_web::{web, HttpRequest, HttpResponse};

fn authorised(req: &HttpRequest) -> bool {
    // Unset or empty denies everything, as in the store: fail closed.
    let expected = match std::env::var("API0_INTERNAL_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => return false,
    };
    req.headers()
        .get("X-Internal-Secret")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == expected)
}

fn leg(result: Result<(), String>) -> serde_json::Value {
    match result {
        Ok(()) => serde_json::json!({ "ok": true, "error": null }),
        Err(e) => serde_json::json!({ "ok": false, "error": e }),
    }
}

pub async fn status(req: HttpRequest, state: web::Data<AppState>) -> HttpResponse {
    if !authorised(&req) {
        return HttpResponse::Unauthorized()
            .json(serde_json::json!({"success": false, "error": "Unauthorized"}));
    }

    let (claude, store) = tokio::join!(state.claude.check_key(), state.store.ping());

    let circuit = match state.claude.circuit_state() {
        CircuitState::Closed => "closed",
        CircuitState::Open => "open",
        CircuitState::HalfOpen => "half_open",
    };

    let now = chrono::Utc::now();

    HttpResponse::Ok().json(serde_json::json!({
        "success": true,
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": state.started_at.to_rfc3339(),
        "uptime_seconds": (now - state.started_at).num_seconds(),
        // What this binary serves. A bridge built before Telegram would not
        // list it — the exact failure the connector test otherwise infers from a 404.
        "channels": ["whatsapp", "telegram"],
        "claude": {
            "model": state.claude.model(),
            "key": leg(claude),
            "circuit": circuit,
        },
        "store": leg(store),
        "webhook_signature_validation": state.meta_app_secret.is_some(),
    }))
}
