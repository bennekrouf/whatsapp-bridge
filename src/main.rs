mod circuit_breaker;
mod claude;
mod config;
pub mod error;
mod mcp_client;
use mcp_client::McpClient;
mod models;
mod rate_limit;
mod status;
mod store_client;
mod telegram_api;
mod telegram_webhook;
mod turn;
mod webhook;
mod whatsapp_api;

use actix_web::{web, App, HttpResponse, HttpServer};
use claude::ClaudeClient;
use config::Config;
use graflog::{app_log, init_logging, LogOption};
use rate_limit::RateLimiter;
use std::sync::Arc;
use store_client::StoreClient;
use whatsapp_api::WhatsAppClient;
use telegram_api::TelegramClient;

pub struct AppState {
    pub store: StoreClient,
    pub mcp: McpClient,
    pub wa: WhatsAppClient,
    pub telegram: TelegramClient,
    pub claude: ClaudeClient,
    pub meta_app_secret: Option<String>,
    pub rate_limiter: RateLimiter,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Accept WhatsApp webhooks with no signature check when neither the tenant
    /// nor the platform has a secret. For local development only.
    pub allow_unsigned_webhooks: bool,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    dotenv::dotenv().ok();

    let log_path = std::env::var("LOG_PATH_API0")
        .unwrap_or_else(|_| "/tmp/whatsapp-bridge.log".to_string());

    init_logging!(&log_path, "api0", "whatsapp-bridge", &[LogOption::Debug]);

    // Load config
    let config_path = std::env::var("CONFIG_PATH")
        .unwrap_or_else(|_| "config.yaml".to_string());

    let config = Config::from_file(&config_path).unwrap_or_else(|e| {
        eprintln!("Failed to load config from {}: {}", config_path, e);
        std::process::exit(1);
    });

    let claude_api_key = std::env::var("CLAUDE_API_KEY").unwrap_or_else(|_| {
        eprintln!("CLAUDE_API_KEY env var required");
        std::process::exit(1);
    });

    let meta_app_secret = std::env::var("META_APP_SECRET").ok().filter(|v| !v.is_empty());
    if meta_app_secret.is_none() {
        app_log!(warn, "META_APP_SECRET not set — WhatsApp tenants without their own App Secret have their messages refused");
    }

    let allow_unsigned_webhooks = std::env::var("ALLOW_UNSIGNED_WEBHOOKS")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    if allow_unsigned_webhooks {
        // Loud on purpose: left on in production, anyone who knows a webhook URL
        // and a linked phone number can run tools as that person.
        app_log!(
            error,
            "ALLOW_UNSIGNED_WEBHOOKS is ON — WhatsApp webhooks without a secret are accepted UNCHECKED. Never use this in production."
        );
    }

    app_log!(info, "WhatsApp bridge starting on {}:{}", config.server.host, config.server.port);
    app_log!(info, "Config: {}", config_path);
    app_log!(info, "Store: {}", config.store.address);
    // Logged because this is the value most likely to be wrong on a deployed
    // host: the gateway's config.yaml says 5009, but its pm2 definition sets
    // API0__SERVER__PORT=50054 and the env override wins. Pointed at the wrong
    // one, linking still succeeds and every tool call fails — so it has to be
    // visible at startup rather than inferred from a failure later.
    app_log!(info, "Gateway: {}", config.gateway.address);
    app_log!(info, "Claude model: {}", config.claude.model);

    let rate_limiter = RateLimiter::new();
    app_log!(info, "Rate limiter initialised");

    let state = Arc::new(AppState {
        store: StoreClient::new(config.store.address.clone()),
        mcp: McpClient::new(&config.gateway.address),
        wa: WhatsAppClient::new(),
        telegram: TelegramClient::new(),
        claude: ClaudeClient::new(
            claude_api_key,
            config.claude.model.clone(),
            config.claude.max_tokens,
        ),
        meta_app_secret,
        rate_limiter,
        started_at: chrono::Utc::now(),
        allow_unsigned_webhooks,
    });

    // Background task: cleanup stale WA sessions once per day
    {
        let store = StoreClient::new(config.store.address.clone());
        tokio::spawn(async move {
            let interval = tokio::time::Duration::from_secs(24 * 60 * 60); // 24h
            // Run first cleanup 60s after startup, then every 24h
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            loop {
                match store.cleanup_stale_sessions(30).await {
                    Ok(n) => app_log!(info, deleted = %n, "Stale session cleanup completed"),
                    Err(e) => app_log!(error, error = %e, "Stale session cleanup failed"),
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    let addr = format!("{}:{}", config.server.host, config.server.port);

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::from(state.clone()))
            .route("/health", web::get().to(|| async {
                HttpResponse::Ok().json(serde_json::json!({"status": "ok"}))
            }))
            // What the admin panel reads: version, and whether Claude and the
            // store are usable from here. Internal-secret only.
            .route("/internal/status", web::get().to(status::status))
            // Meta webhook routes — one URL per tenant
            .route("/webhook/{tenant_id}", web::get().to(webhook::verify))
            .route("/webhook/{tenant_id}", web::post().to(webhook::incoming))
            // Telegram delivers every update here; the bot id routes to a tenant.
            .route("/telegram/webhook/{bot_id}", web::post().to(telegram_webhook::incoming))
    })
    .bind(&addr)?
    .run()
    .await
}
