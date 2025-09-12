# gh-alert

`gh-alert` 用于持续监控 GitHub 上的代码搜索结果，并在结果数量发生变化时通过 Telegram 发送通知。

- **关键词监控**：支持监控多个 GitHub 代码搜索关键词。
- **变更告警**：当搜索结果总数发生变化时，通过 Telegram Bot 发送实时告警信息。
- **尊重 API 限制**：内置速率控制机制，将请求分批发送，确保每分钟不超过 GitHub API 的 10 次限制。
- **状态持久化**：将上一次的搜索结果保存在本地，即使程序重启也能进行正确的比较。

## 使用方法

需要设置以下环境变量：

- **`GH_TOKEN`**: GitHub Personal Access Token，对于 code search 是必须的。
- **`TELEGRAM_BOT_TOKEN`**: 你的 Telegram Bot 的 Token。
- **`TELEGRAM_USER_ID`**: 接收通知的用户的 Chat ID。

命令行：

`gh-alert <terms...> [--interval <SECONDS>] [--tg-token <TOKEN>] [--tg-user-id <ID>]`

- `terms`: (必需) 要监控的一个或多个搜索关键词，用空格分隔。请将包含空格的关键词用引号括起来。
- `--interval <SECONDS>`: (可选) 每次完整搜索周期的间隔时间（秒），默认为 `120`。
- `--tg-token <TOKEN>`: (可选) 覆盖 `TELEGRAM_BOT_TOKEN` 环境变量。
- `--tg-user-id <ID>`: (可选) 覆盖 `TELEGRAM_USER_ID` 环境变量。

### 示例

监控两个关键词，每 300 秒（5 分钟）检查一次。

```bash
gh-alert "proxy" "user:torvalds TODO" --interval 300
```
