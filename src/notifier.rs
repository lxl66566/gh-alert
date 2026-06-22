use anyhow::{Context, Result};
use reqwest::Client;
use serde::Serialize;

const TELEGRAM_API: &str = "https://api.telegram.org";

#[derive(Serialize)]
struct SendMessageRequest<'a> {
    chat_id: &'a str,
    text: &'a str,
}

pub struct TelegramNotifier {
    token: String,
    chat_id: String,
    client: Client,
}

impl TelegramNotifier {
    pub fn new(token: String, chat_id: String, client: Client) -> Self {
        Self {
            token,
            chat_id,
            client,
        }
    }

    pub async fn notify(&self, message: &str) -> Result<()> {
        let url = format!("{TELEGRAM_API}/bot{}/sendMessage", self.token);
        let body = SendMessageRequest {
            chat_id: &self.chat_id,
            text: message,
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("failed to send Telegram request")?;

        resp.error_for_status()
            .context("Telegram API returned an error status")?;

        Ok(())
    }
}
