// src/teams_auth.rs
//
// Proving a request came from Microsoft Teams.
//
// Telegram echoes back a secret we chose. Teams does not: every request carries
// `Authorization: Bearer <JWT>` signed by the Bot Framework, and that token is
// the whole authentication of the endpoint. It is accepted only when:
//
//   - it is signed by a key Microsoft publishes, and that key is endorsed for
//     the "msteams" channel;
//   - it was issued by the Bot Framework, for *this* bot (aud = its App ID),
//     and is in date;
//   - its `serviceurl` claim matches the activity's `serviceUrl`.
//
// The last check matters beyond authentication: the reply is POSTed to
// `serviceUrl` with our own bearer token. Trusting an unverified one would hand
// that token to whoever wrote the request.

use graflog::app_log;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const OPENID_CONFIG_URL: &str = "https://login.botframework.com/v1/.well-known/openidconfiguration";
const ISSUER: &str = "https://api.botframework.com";
const CHANNEL_ID: &str = "msteams";

/// Microsoft rotates keys rarely; a day is what its own SDKs cache them for.
const KEYS_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// A token naming a key we do not hold triggers a refresh, but not more often
/// than this — otherwise forged tokens with random key ids become a way to
/// make us hammer Microsoft.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Microsoft asks for five minutes of clock-skew tolerance.
const LEEWAY_SECS: u64 = 300;

#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    kid: String,
    n: String,
    e: String,
    #[serde(default)]
    endorsements: Vec<String>,
}

#[derive(Deserialize)]
struct KeySet {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct OpenIdConfig {
    jwks_uri: String,
}

#[derive(Debug, Deserialize)]
struct Claims {
    #[serde(alias = "serviceUrl")]
    serviceurl: Option<String>,
}

struct Cached {
    keys: Vec<Jwk>,
    fetched_at: Instant,
}

pub struct TeamsAuth {
    http: reqwest::Client,
    cache: RwLock<Option<Cached>>,
}

impl TeamsAuth {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            cache: RwLock::new(None),
        }
    }

    /// Ok when the request is from Teams, for the bot `app_id`, about
    /// `service_url`. The error is for the log, never for the caller.
    pub async fn verify(&self, authorization: &str, app_id: &str, service_url: &str) -> Result<(), String> {
        let token = authorization
            .strip_prefix("Bearer ")
            .ok_or("no bearer token")?
            .trim();

        let kid = decode_header(token)
            .map_err(|e| format!("unreadable token header: {e}"))?
            .kid
            .ok_or("token names no key")?;

        let jwk = self.key(&kid).await?;
        if !jwk.endorsements.iter().any(|c| c == CHANNEL_ID) {
            return Err(format!("key {kid} is not endorsed for {CHANNEL_ID}"));
        }

        let key = DecodingKey::from_rsa_components(&jwk.n, &jwk.e)
            .map_err(|e| format!("unusable key {kid}: {e}"))?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[app_id]);
        validation.set_issuer(&[ISSUER]);
        validation.leeway = LEEWAY_SECS;

        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|e| format!("token rejected: {e}"))?
            .claims;

        check_service_url(claims.serviceurl.as_deref(), service_url)
    }

    /// The key `kid`, from cache when fresh, fetched otherwise.
    async fn key(&self, kid: &str) -> Result<Jwk, String> {
        {
            let cache = self.cache.read().await;
            if let Some(c) = cache.as_ref() {
                let fresh = c.fetched_at.elapsed() < KEYS_TTL;
                if let Some(k) = c.keys.iter().find(|k| k.kid == kid) {
                    if fresh {
                        return Ok(k.clone());
                    }
                } else if c.fetched_at.elapsed() < MIN_REFRESH_INTERVAL {
                    return Err(format!("unknown key {kid}"));
                }
            }
        }

        let mut cache = self.cache.write().await;
        // Someone else may have refreshed while we waited for the lock.
        let stale = cache
            .as_ref()
            .map_or(true, |c| c.fetched_at.elapsed() >= MIN_REFRESH_INTERVAL);
        if stale {
            match self.fetch_keys().await {
                Ok(keys) => {
                    app_log!(info, count = keys.len(), "Fetched Bot Framework signing keys");
                    *cache = Some(Cached { keys, fetched_at: Instant::now() });
                }
                // Keep serving the old keys rather than locking everyone out
                // because Microsoft blinked.
                Err(e) => app_log!(error, error = %e, "Could not fetch Bot Framework signing keys"),
            }
        }

        cache
            .as_ref()
            .and_then(|c| c.keys.iter().find(|k| k.kid == kid).cloned())
            .ok_or_else(|| format!("unknown key {kid}"))
    }

    async fn fetch_keys(&self) -> Result<Vec<Jwk>, String> {
        let config: OpenIdConfig = self
            .http
            .get(OPENID_CONFIG_URL)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let set: KeySet = self
            .http
            .get(&config.jwks_uri)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        Ok(set.keys)
    }
}

/// The token's `serviceurl` must be the activity's, give or take a trailing
/// slash — Teams is not consistent about it.
fn check_service_url(claimed: Option<&str>, activity: &str) -> Result<(), String> {
    let norm = |s: &str| s.trim_end_matches('/').to_ascii_lowercase();
    match claimed {
        Some(c) if norm(c) == norm(activity) => Ok(()),
        Some(c) => Err(format!("token is for {c}, activity says {activity}")),
        None => Err("token has no serviceurl claim".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::check_service_url;

    #[test]
    fn the_service_url_must_match_the_token() {
        assert!(check_service_url(Some("https://smba.trafficmanager.net/emea/"), "https://smba.trafficmanager.net/emea").is_ok());
        assert!(check_service_url(Some("https://smba.trafficmanager.net/emea/"), "https://evil.example/").is_err());
        assert!(check_service_url(None, "https://smba.trafficmanager.net/emea/").is_err());
    }

    #[tokio::test]
    async fn a_request_without_a_bearer_token_is_refused_before_any_fetch() {
        let auth = super::TeamsAuth::new();
        assert!(auth.verify("", "app", "https://x").await.is_err());
        assert!(auth.verify("Basic abc", "app", "https://x").await.is_err());
        assert!(auth.verify("Bearer not-a-jwt", "app", "https://x").await.is_err());
    }
}
