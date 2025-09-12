use clap::Parser;
use home::home_dir;
use log::{LevelFilter, debug, error, info, warn};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock as Lazy;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, fs, process};

// --- 常量定义 ---
// GitHub API 的速率限制窗口（秒）
const QUERY_INTERVAL_SECONDS: u64 = 60;

static CACHE_FILE_PATH: Lazy<PathBuf> = Lazy::new(|| {
    home_dir()
        .map(|p| p.join(".cache"))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(format!("{}_previous.json", env!("CARGO_PKG_NAME")))
});

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The search terms to look for in GitHub code
    #[clap(num_args = 1..)]
    terms: Vec<String>,

    /// Telegram bot token
    #[arg(long)]
    tg_token: Option<String>,

    /// Telegram destination user ID
    #[arg(long)]
    tg_user_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct GitHubSearchResult {
    total_count: u64,
}

// 定义一个包含结果和速率限制的结构体
struct GitHubResponse {
    count: u64,
    ratelimit_remaining: Option<usize>,
}

#[async_trait::async_trait]
trait Notifier {
    async fn notify(&self, message: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

struct TelegramNotifier {
    token: String,
    user_id: String,
    client: Client,
}

#[async_trait::async_trait]
impl Notifier for TelegramNotifier {
    async fn notify<'a, 'b>(
        &'a self,
        message: &'b str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let mut params = HashMap::new();
        params.insert("chat_id", self.user_id.clone());
        params.insert("text", message.to_string());

        self.client
            .post(&url)
            .header("User-Agent", "gh-alert")
            .json(&params)
            .send()
            .await?;
        Ok(())
    }
}

async fn search_github(client: &Client, term: &str) -> Result<GitHubResponse, reqwest::Error> {
    let mut rb = client
        .get("https://api.github.com/search/code")
        .header("User-Agent", "gh-alert")
        .query(&[("q", term)]);
    if let Ok(gh_token) = env::var("GH_TOKEN") {
        rb = rb.bearer_auth(gh_token);
    }
    let response = rb.send().await?;
    debug!(
        "GitHub search response headers for term '{}': {:?}",
        term,
        response.headers()
    );

    let ratelimit_remaining = response
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());

    if let Some(limit) = ratelimit_remaining {
        debug!("GitHub API ratelimit remaining: {}", limit);
    }
    let search_result: GitHubSearchResult = response.json().await?;
    Ok(GitHubResponse {
        count: search_result.total_count,
        ratelimit_remaining,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    _ = pretty_env_logger::formatted_builder()
        .filter_level(LevelFilter::Info)
        .format_timestamp_secs()
        .parse_default_env()
        .try_init();

    let args = Args::parse();

    if args.terms.is_empty() {
        error!("Error: At least one search term must be provided.");
        process::exit(1);
    }

    let token = args
        .tg_token
        .or_else(|| env::var("TELEGRAM_BOT_TOKEN").ok())
        .expect("TELEGRAM_BOT_TOKEN must be set");
    let user_id = args
        .tg_user_id
        .or_else(|| env::var("TELEGRAM_USER_ID").ok())
        .expect("TELEGRAM_USER_ID must be set");
    let notifier: Arc<dyn Notifier + Send + Sync> = Arc::new(TelegramNotifier {
        token,
        user_id,
        client: Client::new(),
    });

    let previous_results = Arc::new(Mutex::new(load_previous_results(&*CACHE_FILE_PATH)));
    let client = Arc::new(Client::new());

    // 1. 使用一个无限循环的 terms iter 来计算当前查询步骤
    let mut terms_iter = args.terms.iter().map(|s| s.as_str()).cycle().peekable();
    let total_terms = args.terms.len();

    loop {
        info!("--- New query cycle started ---");

        // RTT 1: Peek & Check
        let rate_limit_remaining = if let Some(&peeked_term) = terms_iter.peek() {
            info!("RTT 1: Peeking term '{}' to check rate limit.", peeked_term);
            // 使用 .await 调用异步函数
            match search_github(&client, peeked_term).await {
                Ok(response) => {
                    let remaining = response.ratelimit_remaining.unwrap_or(0);
                    if remaining == 0 {
                        warn!("Rate limit reached (0 remaining). Skipping this cycle.");
                        0
                    } else {
                        info!("Rate limit remaining: {}. Proceeding to RTT 2.", remaining);
                        // 调用异步处理函数
                        process_search_result(
                            peeked_term,
                            response.count,
                            &previous_results,
                            &notifier,
                        )
                        .await;
                        remaining
                    }
                }
                Err(e) => {
                    error!(
                        "RTT 1 Error on peeked term '{}': {}. Skipping cycle.",
                        peeked_term, e
                    );
                    0
                }
            }
        } else {
            error!("Iterator is empty, which should not happen. Exiting.");
            break;
        };

        // RTT 2: Take & Consume Concurrently
        if rate_limit_remaining > 0 {
            // 消耗掉 RTT 1 中 peek 过的 term
            let _ = terms_iter.next();

            // 2. 请求数量是 min(remaining - 1, terms.len - 1)
            let remaining_allowance = rate_limit_remaining.saturating_sub(1);
            let remaining_terms_in_cycle = total_terms.saturating_sub(1);
            let terms_to_take = remaining_allowance.min(remaining_terms_in_cycle);

            if terms_to_take > 0 {
                info!(
                    "RTT 2: Concurrently taking up to {} more terms.",
                    terms_to_take
                );

                let terms_batch: Vec<&str> = terms_iter.by_ref().take(terms_to_take).collect();

                // 创建一个 futures 向量
                let futures = terms_batch.iter().map(|&term| {
                    let client_clone = Arc::clone(&client);
                    async move {
                        // 将 term 和其查询结果关联起来
                        (term, search_github(&client_clone, term).await)
                    }
                });

                // 1. 使用异步查询，同时发送请求
                let results = futures::future::join_all(futures).await;

                // 异步处理所有结果
                for (term, result) in results {
                    match result {
                        Ok(response) => {
                            process_search_result(
                                term,
                                response.count,
                                &previous_results,
                                &notifier,
                            )
                            .await;
                        }
                        Err(e) => {
                            error!("RTT 2 Error searching for '{}': {}", term, e);
                        }
                    }
                }
            } else {
                info!("RTT 2: No more terms to take in this cycle or no rate limit left.");
            }
        }

        // 使用 tokio 的异步 sleep
        info!(
            "Query cycle finished. Waiting for {} seconds.",
            QUERY_INTERVAL_SECONDS
        );
        tokio::time::sleep(Duration::from_secs(QUERY_INTERVAL_SECONDS)).await;
    }

    Ok(())
}

// 辅助函数，处理查询结果的比较、通知和保存逻辑
async fn process_search_result(
    term: &str,
    current_count: u64,
    previous_results: &Arc<Mutex<HashMap<String, u64>>>,
    notifier: &Arc<dyn Notifier + Send + Sync>,
) {
    // 将所有需要访问锁的操作都放在这个块中
    let changed = {
        let mut results = previous_results.lock().unwrap();
        let previous_count = results.get(term).cloned().unwrap_or(0);

        if previous_count != current_count {
            results.insert(term.to_string(), current_count);
            if let Err(e) = save_results(&*CACHE_FILE_PATH, &results) {
                error!("Failed to save results: {}", e);
            }
            Some(previous_count) // 返回 Some(previous_count) 表示发生了变化
        } else {
            info!("No change for '{}', count remains {}.", term, current_count);
            None // 返回 None 表示没有变化
        }
    };
    if let Some(previous_count) = changed {
        let message = format!(
            "GitHub search result for '{}' has changed from {} to {}.",
            term, previous_count, current_count
        );
        info!("{}", message);
        if let Err(e) = notifier.notify(&message).await {
            error!("Failed to send notification: {}", e);
        }
    }
}

fn load_previous_results(file_path: impl AsRef<Path>) -> HashMap<String, u64> {
    if file_path.as_ref().exists() {
        // 使用 unwrap_or_else 来处理文件读取或解析失败的情况
        fs::File::open(&file_path)
            .and_then(|mut file| {
                let mut contents = String::new();
                file.read_to_string(&mut contents)?;
                Ok(contents)
            })
            .and_then(|contents| {
                serde_json::from_str(&contents)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })
            .unwrap_or_else(|e| {
                warn!(
                    "Failed to load or parse previous results from {:?}: {}. Starting fresh.",
                    file_path.as_ref(),
                    e
                );
                HashMap::new()
            })
    } else {
        warn!("No previous results file found, starting from scratch.");
        HashMap::new()
    }
}

fn save_results(
    file_path: impl AsRef<Path>,
    results: &HashMap<String, u64>,
) -> Result<(), std::io::Error> {
    // 确保缓存目录存在
    if let Some(parent) = file_path.as_ref().parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::File::create(file_path)?;
    let contents = serde_json::to_string_pretty(results)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Client;
    use std::env;

    #[ignore = "requires env vars"]
    #[tokio::test]
    async fn test_telegram_notifier_sends_message() {
        let token =
            env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN must be set for this test");
        let user_id =
            env::var("TELEGRAM_USER_ID").expect("TELEGRAM_USER_ID must be set for this test");

        let client = Client::new();
        let notifier = TelegramNotifier {
            token,
            user_id,
            client,
        };

        let test_message = "This is an automated test message from the Rust GitHub Alerter.";
        let result = notifier.notify(test_message).await;

        assert!(
            result.is_ok(),
            "Failed to send Telegram notification: {:?}",
            result.err()
        );

        info!("Successfully sent test message to Telegram. Please check your client.");
    }
}
