// src/turn.rs
//
// One conversational turn, for any messaging channel.
//
// A channel adapter's whole job is: parse an inbound update into (who, what),
// call `run_turn`, send the reply back. Everything that makes the bridge worth
// having — linking a person, acting as them through the gateway, remembering
// the conversation — lives here once, so a new channel is a thin adapter and
// not a second copy of the product.

use crate::error::{BridgeError, BridgeResult};
use crate::rate_limit::RateLimitDenied;
use crate::AppState;
use graflog::app_log;

/// What an adapter has to tell us about an inbound message.
pub struct Inbound<'a> {
    /// "whatsapp", "telegram", … — the identity namespace.
    pub channel: &'a str,
    /// What the platform calls the sender. Stable, opaque.
    pub external_id: &'a str,
    pub tenant_id: &'a str,
    pub system_prompt: &'a str,
    pub text: &'a str,
}

/// A link code is six characters from an unambiguous alphabet. A message that
/// is exactly that, give or take whitespace and case, is treated as one.
pub fn looks_like_link_code(text: &str) -> Option<String> {
    let t = text.trim().to_uppercase();
    let ok = t.len() == 6
        && t.bytes().all(|b| b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(&b));
    if ok { Some(t) } else { None }
}

const LINK_INSTRUCTIONS: &str = "This account isn't linked yet.\n\n\
Sign in at the dashboard, open Settings → Linked messaging, and send me the \
6-character code it shows you. After that, everything you ask here runs as you.";

/// Run one turn and return the text to send back.
///
/// Rate limiting and identity are resolved here so an adapter cannot forget
/// them. The only reason to return `Err` is something the adapter should record
/// as a failure; anything a person can act on comes back as the reply.
pub async fn run_turn(state: &AppState, m: Inbound<'_>) -> BridgeResult<String> {
    // Sessions and limits are per (tenant, sender), and the sender key is
    // namespaced by channel so a Telegram id can never collide with a phone.
    let sender_key = format!("{}:{}", m.channel, m.external_id);

    if let Err(denied) = state.rate_limiter.check_and_record(m.tenant_id, &sender_key).await {
        let reason = match denied {
            RateLimitDenied::Phone => "per-sender",
            RateLimitDenied::Tenant => "per-tenant",
        };
        app_log!(warn, tenant_id = %m.tenant_id, channel = %m.channel, limit = %reason, "Rate limited");
        return Ok("You're sending messages too quickly. Please wait a moment and try again.".into());
    }

    // ── Who is this? ────────────────────────────────────────────────────────
    // A platform id is not an api0 person. Someone becomes one by sending a
    // code minted in the dashboard; from then on their own key is used, so every
    // tool call carries their own credentials and their own name.
    let identity = match state
        .store
        .resolve_identity(m.channel, m.external_id, m.tenant_id)
        .await?
    {
        Some(id) => id,
        None => {
            let reply = match looks_like_link_code(m.text) {
                Some(code) => match state
                    .store
                    .redeem_link_code(m.channel, m.external_id, m.tenant_id, &code)
                    .await
                {
                    Ok(email) => format!("Linked. You are now {} here — anything you ask runs as you.", email),
                    Err(BridgeError::Store { message, .. }) => message,
                    Err(e) => return Err(e),
                },
                None => LINK_INSTRUCTIONS.to_string(),
            };
            return Ok(reply);
        }
    };

    app_log!(info, tenant_id = %m.tenant_id, channel = %m.channel, user = %identity.user_email, "Acting as linked person");

    let history = state.store.get_session(m.tenant_id, &sender_key).await?;
    let tools = state.mcp.list_tools(&identity.api_key).await?;

    let system = if m.system_prompt.trim().is_empty() {
        "You are a helpful assistant. Use the available tools to answer the user's request."
    } else {
        m.system_prompt
    };

    let (reply, updated_history) = match state
        .claude
        .run(system, history, m.text, &tools, &state.mcp, &identity.api_key)
        .await
    {
        Ok(r) => r,
        Err(BridgeError::CircuitOpen) => {
            app_log!(warn, tenant_id = %m.tenant_id, "Circuit open");
            return Ok("I'm temporarily unavailable. Please try again in a few moments.".into());
        }
        Err(e) => return Err(e),
    };

    state.store.save_session(m.tenant_id, &sender_key, &updated_history).await?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::looks_like_link_code;

    #[test]
    fn a_six_character_code_is_recognised_whatever_the_case_or_spacing() {
        assert_eq!(looks_like_link_code("abc234"), Some("ABC234".into()));
        assert_eq!(looks_like_link_code("  KLMN78 "), Some("KLMN78".into()));
    }

    #[test]
    fn ordinary_messages_are_not_mistaken_for_codes() {
        for text in ["hello", "ABCDE", "ABCDEFG", "AB0123", "list my tasks", "OI1LO0"] {
            assert_eq!(looks_like_link_code(text), None, "{text:?} should not be a code");
        }
    }
}
