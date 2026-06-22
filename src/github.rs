use std::{
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use log::{debug, info, warn};
use reqwest::{Client, StatusCode, header::HeaderMap};
use serde::Deserialize;

const SEARCH_URL: &str = "https://api.github.com/search/code";
const MAX_ATTEMPTS: u32 = 3;
const INITIAL_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Deserialize)]
struct SearchResponse {
    total_count: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct RateLimit {
    pub remaining: Option<u32>,
    pub reset_at: Option<SystemTime>,
}

impl RateLimit {
    fn parse(headers: &HeaderMap) -> Self {
        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok());

        let reset_at = headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));

        Self {
            remaining,
            reset_at,
        }
    }
}

pub struct SearchOutcome {
    pub count: u64,
    pub ratelimit: RateLimit,
}

pub struct GitHubClient {
    client: Client,
    token: Option<String>,
}

impl GitHubClient {
    pub fn new(client: Client, token: Option<String>) -> Self {
        Self { client, token }
    }

    /// Searches GitHub for `term`, retrying transient failures with backoff.
    pub async fn search(&self, term: &str) -> Result<SearchOutcome> {
        let mut backoff = INITIAL_BACKOFF;

        for attempt in 1..MAX_ATTEMPTS {
            match self.search_once(term).await {
                Ok(outcome) => return Ok(outcome),
                Err(e) if is_retryable(&e) => {
                    warn!(
                        "Attempt {attempt}/{MAX_ATTEMPTS} for '{term}' failed ({e}); \
                         retrying in {backoff:?}."
                    );
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("GitHub search for '{term}'"));
                }
            }
        }

        self.search_once(term)
            .await
            .with_context(|| format!("GitHub search for '{term}'"))
    }

    async fn search_once(&self, term: &str) -> std::result::Result<SearchOutcome, reqwest::Error> {
        let mut req = self
            .client
            .get(SEARCH_URL)
            .header("User-Agent", "gh-alert")
            .header("Accept", "application/vnd.github+json")
            .query(&[("q", term)]);

        if let Some(token) = self.token.as_ref() {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await?;
        let ratelimit = RateLimit::parse(resp.headers());
        debug!(
            "'{term}' -> {} (rate limit: {:?})",
            resp.status(),
            ratelimit
        );

        let resp = resp.error_for_status()?;
        let body: SearchResponse = resp.json().await?;

        Ok(SearchOutcome {
            count: body.total_count,
            ratelimit,
        })
    }
}

/// Resolves the GitHub token from `GH_TOKEN`, falling back to `gh auth token`.
pub fn resolve_token() -> Option<String> {
    if let Ok(token) = std::env::var("GH_TOKEN")
        && !token.is_empty()
    {
        debug!("Using token from GH_TOKEN.");
        return Some(token);
    }

    match Command::new("gh").args(["auth", "token"]).output() {
        Ok(output) if output.status.success() => {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if token.is_empty() {
                debug!("`gh auth token` succeeded but returned an empty value.");
                None
            } else {
                info!("Using token from `gh auth token`.");
                Some(token)
            }
        }
        Ok(_) => {
            debug!("`gh auth token` failed; gh CLI may not be authenticated.");
            None
        }
        Err(e) => {
            debug!("Could not run `gh` ({e}); is GitHub CLI installed?");
            None
        }
    }
}

fn is_retryable(e: &reqwest::Error) -> bool {
    if e.is_timeout() || e.is_connect() {
        return true;
    }
    match e.status() {
        Some(StatusCode::TOO_MANY_REQUESTS) => true,
        Some(status) if status.is_server_error() => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    #[test]
    fn parse_rate_limit_from_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("42"));
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("1700000000"));

        let rl = RateLimit::parse(&headers);

        assert_eq!(rl.remaining, Some(42));
        assert_eq!(
            rl.reset_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
    }

    #[test]
    fn parse_rate_limit_missing_headers() {
        let rl = RateLimit::parse(&HeaderMap::new());

        assert_eq!(rl.remaining, None);
        assert_eq!(rl.reset_at, None);
    }
}
