// src/telegram_api.rs
//
// The two Telegram Bot API calls the bridge needs. Everything is
// https://api.telegram.org/bot<token>/<method>, JSON in, JSON out.

use crate::error::{BridgeError, BridgeResult};
use graflog::app_log;

/// Telegram caps a message at 4096 characters. Longer replies are split rather
/// than dropped, on paragraph boundaries where possible.
const MAX_MESSAGE_CHARS: usize = 4096;

pub struct TelegramClient {
    http: reqwest::Client,
}

impl TelegramClient {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }

    pub async fn send_text(&self, bot_token: &str, chat_id: i64, text: &str) -> BridgeResult<()> {
        for chunk in split_message(text, MAX_MESSAGE_CHARS) {
            let url = format!("https://api.telegram.org/bot{}/sendMessage", bot_token);
            let resp = self
                .http
                .post(&url)
                .json(&serde_json::json!({ "chat_id": chat_id, "text": chunk }))
                .send()
                .await
                .map_err(BridgeError::TelegramNetwork)?;

            let status = resp.status().as_u16();
            if !(200..300).contains(&status) {
                let body = resp.text().await.unwrap_or_default();
                app_log!(error, status = status, body = %body, "Telegram sendMessage failed");
                return Err(BridgeError::TelegramApi { status, body });
            }
        }
        Ok(())
    }
}

/// Split a reply into messages of at most `max` characters, on paragraph
/// boundaries where possible. Shared by every channel with a size cap.
pub fn split_message(text: &str, max: usize) -> Vec<String> {
    if text.chars().count() <= max {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for para in text.split("\n\n") {
        let candidate_len = current.chars().count() + para.chars().count() + 2;
        if candidate_len > max && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        // A single paragraph longer than the cap is cut hard; rare enough.
        current.extend(para.chars().take(max));
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_messages_are_sent_whole() {
        assert_eq!(split_message("hello", MAX_MESSAGE_CHARS), vec!["hello".to_string()]);
    }

    #[test]
    fn long_messages_split_on_paragraphs_under_the_cap() {
        let para = "x".repeat(3000);
        let text = format!("{para}\n\n{para}");
        let parts = split_message(&text, MAX_MESSAGE_CHARS);
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|p| p.chars().count() <= MAX_MESSAGE_CHARS));
    }
}
