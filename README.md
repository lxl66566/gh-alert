# gh-alert

`gh-alert` 持续监控 GitHub 代码搜索结果，当结果数量变化时通过 Telegram 发送通知。

- **关键词监控**：支持同时监控多个 GitHub 代码搜索关键词。
- **变更告警**：当搜索结果总数发生变化时，通过 Telegram Bot 实时推送。
- **认证灵活**：优先读取 `GH_TOKEN`，找不到时自动回退到 `gh auth token`（GitHub CLI）。
- **尊重速率限制**：根据 `x-ratelimit-remaining` / `x-ratelimit-reset` 动态调整下一次查询的时间。
- **状态持久化**：结果缓存在本地（原子写入），重启后可正确比较。

## 认证

GitHub code search 必须认证，按以下顺序解析 token：

1. 环境变量 **`GH_TOKEN`**
2. `gh auth token`（需安装并登录 [GitHub CLI](https://cli.github.com/)，执行 `gh auth login`）

二者任选其一即可。

## 使用方法

### 环境变量

| 变量 | 说明 |
| --- | --- |
| `GH_TOKEN` | （可选）GitHub PAT，未设置时回退到 `gh auth token`。 |
| `TELEGRAM_BOT_TOKEN` | Telegram Bot Token。 |
| `TELEGRAM_USER_ID` | 接收通知的 Chat ID。 |
| `RUST_LOG` | 日志级别，如 `debug`。 |

### 命令行

```
gh-alert [OPTIONS] <TERMS>...
```

**参数：**

- `<TERMS>...`：要监控的搜索关键词（至少一个）。

**选项：**

| 选项 | 默认 | 说明 |
| --- | --- | --- |
| `-i, --interval <SECS>` | `60` | 两次查询循环之间的间隔（秒）。 |
| `--once` | — | 只跑一轮，完成后退出。 |
| `--tg-token <TOKEN>` | — | 覆盖 `TELEGRAM_BOT_TOKEN`（**注意：会暴露在进程列表中**）。 |
| `--tg-user-id <ID>` | — | 覆盖 `TELEGRAM_USER_ID`。 |
| `-h, --help` | — | 帮助。 |
| `-V, --version` | — | 版本。 |

> 安全提示：通过 `--tg-token` 传递的密钥会被其他用户通过 `ps` 等方式看到，建议优先使用环境变量。

### 示例

```bash
# 持续监控（默认 60 秒一轮）
gh-alert "proxy" "user:torvalds TODO"

# 只跑一轮（适合 cron / 定时任务）
gh-alert --once "proxy"

# 自定义间隔
gh-alert --interval 120 "proxy"
```
