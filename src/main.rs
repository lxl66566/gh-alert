use clap::Parser;
use home::home_dir;
use log::{LevelFilter, debug, error, info, warn};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock as Lazy;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, fs, process, thread};
// 注意：移除了 threadpool 依赖，因为它不再被需要。

// 每分钟最多请求数
const REQUESTS_PER_MINUTE: usize = 10;
// GitHub API 的时间窗口（秒）
const RATE_LIMIT_WINDOW_SECONDS: u64 = 60;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The search terms to look for in GitHub code
    #[clap(num_args = 1..)]
    terms: Vec<String>,

    /// The interval in seconds between each full search cycle.
    #[arg(short, long, default_value_t = 120)]
    interval: u64,

    /// Telegram bot token
    #[arg(long)]
    tg_token: Option<String>,

    /// Telegram destination user ID
    #[arg(long)]
    tg_user_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct GitHubSearchResult {
    total_count: u64,
}

trait Notifier {
    fn notify(&self, message: &str) -> Result<(), Box<dyn std::error::Error>>;
}

struct TelegramNotifier {
    token: String,
    user_id: String,
    client: Client,
}

impl Notifier for TelegramNotifier {
    fn notify(&self, message: &str) -> Result<(), Box<dyn std::error::Error>> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let mut params = HashMap::new();
        params.insert("chat_id", self.user_id.clone());
        params.insert("text", message.to_string());

        self.client
            .post(&url)
            .header("User-Agent", "gh-alert")
            .json(&params)
            .send()?;
        Ok(())
    }
}

fn search_github(client: &Client, term: &str) -> Result<u64, reqwest::Error> {
    let url = format!("https://api.github.com/search/code?q={}", term);
    let mut rb = client.get(&url).header("User-Agent", "gh-alert");
    if let Ok(gh_token) = env::var("GH_TOKEN") {
        rb = rb.bearer_auth(gh_token);
    }
    let response = rb.send()?;
    debug!("GitHub search response: {:?}", response);

    // GitHub 的速率限制响应头通常包含在 Header 中
    if let Some(limit) = response.headers().get("x-ratelimit-remaining") {
        debug!("GitHub API ratelimit remaining: {:?}", limit);
    }

    Ok(response.json::<GitHubSearchResult>()?.total_count)
}

static CACHE_FILE_PATH: Lazy<PathBuf> = Lazy::new(|| {
    home_dir()
        .map(|p| p.join(".cache"))
        .unwrap_or(PathBuf::from("."))
        .join(format!("{}_previous.json", env!("CARGO_PKG_NAME")))
});

fn main() -> Result<(), Box<dyn std::error::Error>> {
    _ = pretty_env_logger::formatted_builder()
        .filter_level(LevelFilter::Info)
        .format_timestamp_secs()
        .parse_default_env()
        .try_init();

    let args = Args::parse();

    // --- 新增：检查 interval 参数 ---
    let at_least = args.terms.len() as u64 * RATE_LIMIT_WINDOW_SECONDS / 10;
    if args.interval < at_least {
        error!(
            "Error: The interval must be at lease {} for {} terms to respect the GitHub rate limit when monitoring multiple terms.",
            at_least,
            args.terms.len()
        );
        process::exit(1);
    }

    let token = args
        .tg_token
        .or(env::var("TELEGRAM_BOT_TOKEN").ok())
        .expect("TELEGRAM_BOT_TOKEN must be set");
    let user_id = args
        .tg_user_id
        .or(env::var("TELEGRAM_USER_ID").ok())
        .expect("TELEGRAM_USER_ID must be set");
    let notifier: Arc<dyn Notifier + Send + Sync> = Arc::new(TelegramNotifier {
        token,
        user_id,
        client: Client::new(),
    });

    let previous_results = Arc::new(Mutex::new(load_previous_results(&*CACHE_FILE_PATH)));
    let client = Arc::new(Client::new());

    loop {
        let cycle_start_time = Instant::now();
        info!(
            "Starting a new search cycle for {} terms.",
            args.terms.len()
        );

        // 将所有 term 分块，每块的大小为 REQUESTS_PER_MINUTE
        let chunks: Vec<_> = args.terms.chunks(REQUESTS_PER_MINUTE).collect();
        let total_chunks = chunks.len();

        for (i, chunk) in chunks.into_iter().enumerate() {
            info!("Processing chunk {}/{}...", i + 1, total_chunks);
            for term in chunk {
                match search_github(&client, term) {
                    Ok(current_count) => {
                        let mut results = previous_results.lock().unwrap();
                        let previous_count = results.get(term).cloned().unwrap_or(0);

                        if previous_count != current_count {
                            let message = format!(
                                "GitHub search result for '{}' has changed from {} to {}.",
                                term, previous_count, current_count
                            );
                            info!("{}", message);
                            if let Err(e) = notifier.notify(&message) {
                                error!("Failed to send notification: {}", e);
                            }
                            results.insert(term.to_string(), current_count);
                            if let Err(e) = save_results(&*CACHE_FILE_PATH, &results) {
                                error!("Failed to save results: {}", e);
                            }
                        } else {
                            info!("No change for '{}', count remains {}.", term, current_count);
                        }
                    }
                    Err(e) => {
                        // 如果请求失败，可能是因为达到了速率限制，打印错误并继续
                        error!("Error searching for '{}': {}", term, e);
                    }
                }
            }

            // 如果这不是最后一个块，就等待60秒，为下一批请求留出时间窗口
            if i < total_chunks - 1 {
                info!(
                    "Finished chunk {}/{}. Waiting for {} seconds before next chunk...",
                    i + 1,
                    total_chunks,
                    RATE_LIMIT_WINDOW_SECONDS
                );
                thread::sleep(Duration::from_secs(RATE_LIMIT_WINDOW_SECONDS));
            }
        }

        info!("Search cycle completed.");

        // 计算整个周期花费的时间，并从用户设定的 interval 中减去，以实现精确的周期性执行
        let elapsed = cycle_start_time.elapsed();
        let interval_duration = Duration::from_secs(args.interval);

        if elapsed < interval_duration {
            let sleep_duration = interval_duration - elapsed;
            info!(
                "Waiting for {:?} before starting the next cycle.",
                sleep_duration
            );
            thread::sleep(sleep_duration);
        }
    }
}

fn load_previous_results(file_path: impl AsRef<Path>) -> HashMap<String, u64> {
    if file_path.as_ref().exists() {
        let mut file = fs::File::open(file_path).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        serde_json::from_str(&contents).unwrap_or_default()
    } else {
        warn!("No previous results found, starting from scratch.");
        HashMap::new()
    }
}

fn save_results(
    file_path: impl AsRef<Path>,
    results: &HashMap<String, u64>,
) -> Result<(), std::io::Error> {
    let mut file = fs::File::create(file_path)?;
    let contents = serde_json::to_string_pretty(results)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::blocking::Client;
    use std::env;

    #[ignore = "requires env vars"]
    #[test]
    fn test_telegram_notifier_sends_message() {
        // 从环境变量中读取 Telegram Bot Token 和 User ID
        // 在运行测试前，你需要设置好这两个环境变量
        // 例如: export TELEGRAM_BOT_TOKEN="your_token"
        //       export TELEGRAM_USER_ID="your_user_id"
        let token =
            env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN must be set for this test");
        let user_id =
            env::var("TELEGRAM_USER_ID").expect("TELEGRAM_USER_ID must be set for this test");

        // 创建一个 reqwest 客户端实例
        let client = Client::new();

        // 实例化 TelegramNotifier
        let notifier = TelegramNotifier {
            token,
            user_id,
            client,
        };

        // 定义测试消息
        let test_message = "This is an automated test message from the Rust GitHub Alerter.";

        // 调用 notify 方法并断言结果
        let result = notifier.notify(test_message);

        // 检查 API 调用是否成功
        // 如果 notify 方法返回 Err，测试将会失败并打印出错误信息
        assert!(
            result.is_ok(),
            "Failed to send Telegram notification: {:?}",
            result.err()
        );

        // 如果程序执行到这里，说明消息已成功发送（HTTP请求已成功）
        // 你应该在你的 Telegram 中检查是否收到了这条测试消息
        info!("Successfully sent test message to Telegram. Please check your client.");
    }
}
