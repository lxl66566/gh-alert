mod cache;
mod github;
mod notifier;

use std::{
    path::PathBuf,
    sync::LazyLock,
    time::{Duration, SystemTime},
};

use anyhow::Context;
use cache::Results;
use clap::Parser;
use futures_util::stream::{self, StreamExt};
use github::{GitHubClient, SearchOutcome};
use log::{LevelFilter, debug, error, info, warn};
use reqwest::Client;

const DEFAULT_INTERVAL_SECS: u64 = 60;
const CONCURRENCY: usize = 5;
const HTTP_TIMEOUT_SECS: u64 = 30;

static CACHE_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gh-alert")
        .join("previous.json")
});

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Search terms to monitor on GitHub code search.
    #[arg(num_args = 1..)]
    terms: Vec<String>,

    /// Seconds to wait between query cycles.
    #[arg(short, long, default_value_t = DEFAULT_INTERVAL_SECS)]
    interval: u64,

    /// Run a single cycle then exit.
    #[arg(long)]
    once: bool,

    /// Telegram bot token (overrides `TELEGRAM_BOT_TOKEN`).
    ///
    /// Note: passing secrets via the command line exposes them in the process
    /// list. Prefer the `TELEGRAM_BOT_TOKEN` environment variable.
    #[arg(long)]
    tg_token: Option<String>,

    /// Telegram destination user/chat ID (overrides `TELEGRAM_USER_ID`).
    #[arg(long)]
    tg_user_id: Option<String>,
}

struct SearchAttempt {
    term: String,
    result: anyhow::Result<SearchOutcome>,
}

struct Change {
    term: String,
    previous: u64,
    current: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = pretty_env_logger::formatted_builder()
        .filter_level(LevelFilter::Info)
        .format_timestamp_secs()
        .parse_default_env()
        .try_init();

    let args = Args::parse();

    let http_client = Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("failed to build HTTP client")?;

    let gh_token = github::resolve_token();
    if gh_token.is_none() {
        warn!(
            "No GitHub token found. Set GH_TOKEN or run `gh auth login`. \
             Unauthenticated code search is heavily rate-limited and may fail."
        );
    }
    let gh_client = GitHubClient::new(http_client.clone(), gh_token);

    let tg_token = args
        .tg_token
        .or_else(|| std::env::var("TELEGRAM_BOT_TOKEN").ok())
        .filter(|s| !s.is_empty())
        .context("TELEGRAM_BOT_TOKEN is required (set the env var or pass --tg-token)")?;
    let tg_user_id = args
        .tg_user_id
        .or_else(|| std::env::var("TELEGRAM_USER_ID").ok())
        .filter(|s| !s.is_empty())
        .context("TELEGRAM_USER_ID is required (set the env var or pass --tg-user-id)")?;

    let notifier = notifier::TelegramNotifier::new(tg_token, tg_user_id, http_client);

    let mut results = cache::load(&CACHE_PATH);

    run_loop(
        &gh_client,
        &notifier,
        &args.terms,
        args.interval,
        args.once,
        &mut results,
    )
    .await;

    Ok(())
}

async fn run_loop(
    gh_client: &GitHubClient,
    notifier: &notifier::TelegramNotifier,
    terms: &[String],
    interval_secs: u64,
    once: bool,
    results: &mut Results,
) {
    let interval = Duration::from_secs(interval_secs);

    loop {
        info!("--- Query cycle: {} term(s) ---", terms.len());

        let attempts = search_all(gh_client, terms).await;
        let changes = process_changes(&attempts, results);
        notify_changes(notifier, &changes).await;

        if !changes.is_empty() {
            if let Err(e) = cache::save(&CACHE_PATH, results) {
                error!("Failed to save cache: {e:#}");
            }
        }

        if once {
            info!("--once: exiting after a single cycle.");
            break;
        }

        let sleep_dur = compute_sleep(&attempts, interval);
        info!("Next cycle in {sleep_dur:?}.");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Interrupt received, shutting down.");
                break;
            }
            () = tokio::time::sleep(sleep_dur) => {}
        }
    }
}

/// Queries all terms concurrently (bounded by [`CONCURRENCY`]).
async fn search_all(client: &GitHubClient, terms: &[String]) -> Vec<SearchAttempt> {
    stream::iter(terms.iter().cloned())
        .map(|term| async move {
            let result = client.search(&term).await;
            SearchAttempt { term, result }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await
}

/// Compares each result against the cache, updating it and collecting changes.
fn process_changes(attempts: &[SearchAttempt], results: &mut Results) -> Vec<Change> {
    let mut changes = Vec::new();
    for attempt in attempts {
        let term = attempt.term.as_str();
        match &attempt.result {
            Ok(outcome) => {
                let previous = results.get(term).copied().unwrap_or(0);
                if previous == outcome.count {
                    debug!("'{term}': still {previous} (no change)");
                } else {
                    results.insert(term.to_string(), outcome.count);
                    info!("'{term}': {previous} -> {} (changed)", outcome.count);
                    changes.push(Change {
                        term: term.to_string(),
                        previous,
                        current: outcome.count,
                    });
                }
            }
            Err(e) => {
                error!("Search for '{term}' failed: {e:#}");
            }
        }
    }
    changes
}

async fn notify_changes(notifier: &notifier::TelegramNotifier, changes: &[Change]) {
    for change in changes {
        let message = format!(
            "GitHub search for '{}' changed: {} -> {}.",
            change.term, change.previous, change.current
        );
        info!("{message}");
        if let Err(e) = notifier.notify(&message).await {
            error!("Telegram notification failed: {e:#}");
        }
    }
}

/// If any response indicates the rate-limit budget is exhausted, sleeps until
/// the reset time reported by GitHub; otherwise returns `default`.
fn compute_sleep(attempts: &[SearchAttempt], default: Duration) -> Duration {
    let furthest_reset = attempts
        .iter()
        .filter_map(|a| a.result.as_ref().ok())
        .filter(|o| o.ratelimit.remaining == Some(0))
        .filter_map(|o| o.ratelimit.reset_at)
        .max();

    match furthest_reset {
        Some(reset) => reset
            .duration_since(SystemTime::now())
            .map(|d| d + Duration::from_secs(1))
            .unwrap_or(default),
        None => default,
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use github::RateLimit;

    use super::*;

    fn attempt(remaining: Option<u32>, reset_at: Option<SystemTime>) -> SearchAttempt {
        SearchAttempt {
            term: "test".to_string(),
            result: Ok(SearchOutcome {
                count: 0,
                ratelimit: RateLimit {
                    remaining,
                    reset_at,
                },
            }),
        }
    }

    #[test]
    fn compute_sleep_default_when_no_data() {
        let attempts = vec![attempt(None, None)];
        assert_eq!(
            compute_sleep(&attempts, Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn compute_sleep_default_when_budget_left() {
        let attempts = vec![attempt(Some(5), None)];
        assert_eq!(
            compute_sleep(&attempts, Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn compute_sleep_waits_until_reset_when_exhausted() {
        let reset = UNIX_EPOCH
            + Duration::from_secs(
                (SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs())
                    + 10,
            );
        let attempts = vec![attempt(Some(0), Some(reset))];

        let sleep = compute_sleep(&attempts, Duration::from_secs(60));

        assert!(sleep >= Duration::from_secs(8));
        assert!(sleep <= Duration::from_secs(12));
    }
}
