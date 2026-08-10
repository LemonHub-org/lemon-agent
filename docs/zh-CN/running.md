# 运行手册 (Running Lemon Agent)

Lemon Agent 是一个单个轻量二进制加上一个 `scripts/` 目录和一个 TOML
配置文件。本指南涵盖安装、配置，以及两种支持的部署方式（Docker 和
systemd）。

## 前置条件

- Rust 1.97+（仅用于从源码构建；发布版直接提供二进制）。
- 宿主机或容器中安装了 `git`：agent 用它对已验证的变更进行暂存和提交。
- 一个 OpenAI 兼容的 API 端点。

## 构建

```bash
cargo build --release
# 二进制: target/release/lemon-agent
```

本地提交前先验证工具链门禁：

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

## 配置

复制 `config.toml` 并设置 LLM 端点。机密从不写入文件：

| 设置项 | 环境变量覆盖 | CLI 参数 |
|---------|------------------|----------|
| `llm.provider` | `AGENT_LLM_PROVIDER` | `--llm-provider` |
| `llm.api_key` | `AGENT_API_KEY` | `--api-key` |
| `llm.base_url` | `AGENT_LLM_BASE_URL` | `--llm-base-url` |
| `llm.model` | `AGENT_MODEL` | `--model` |
| `agent.work_dir` | `AGENT_WORK_DIR` | `--work-dir` |
| `agent.db_path` | `AGENT_DB_PATH` | `--db-path` |
| `logging.level` | `AGENT_LOG_LEVEL` | — |

优先级：配置文件 < 环境变量 < CLI 参数。所有无效设置会在开始任何工作
之前于启动时报告。

最小 `config.toml`：

```toml
[agent]
work_dir = "./workspace"
scripts_dir = "./scripts"

[llm]
provider = "openai"          # openai | anthropic | gemini | custom
api_key = ""                 # 使用 AGENT_API_KEY
base_url = "https://api.openai.com/v1"
model = "gpt-4"

[sandbox]
root_dir = "./workspace"
allowed_commands = ["git", "cargo", "rustc", "python3", "ls"]
```

## LLM 提供商

LLM 网关通过可插拔适配器支持四种提供商：

| 提供商 | 协议 | 说明 |
|----------|----------|-------|
| `openai`（默认） | OpenAI chat completions | 通过 `base_url` 同样覆盖 DeepSeek、Ollama、vLLM 及任意 OpenAI 兼容端点。 |
| `anthropic` | Messages API | 使用 `x-api-key` + `anthropic-version` 请求头；`max_output_tokens` 为必填并会发送。 |
| `gemini` | GenerateContent | 使用 `x-goog-api-key`；模型名嵌入请求路径。 |
| `custom` | 可自定义的 OpenAI 兼容端点 | 用于自建网关、代理和 LLM 聚合器。 |

所有提供商都支持流式响应、工具调用、退避重试和超时。规范化后的消息与
工具模型在所有提供商之间完全一致。

示例：通过 Anthropic 使用 Claude

```toml
[llm]
provider = "anthropic"
base_url = "https://api.anthropic.com"
model = "claude-3-5-sonnet-20241022"
max_output_tokens = 8192
```

示例：自定义 OpenAI 兼容网关（如 OneAPI 风格的代理）

```toml
[llm]
provider = "custom"
base_url = "https://gateway.example.com"
model = "deepseek-chat"

[llm.custom]
chat_path = "/v1/chat/completions"
api_key_header = "X-Api-Key"
api_key_scheme = ""
headers = { "X-Tenant" = "team-a" }
```

## 运行

```bash
./target/release/lemon-agent \
  --config config.toml \
  --task "为这个项目添加速率限制并测试"
```

agent 进行规划，通过 `scripts/plan_and_execute.rhai` 执行，验证结果，
把每个事件持久化到 `agent.db`，并打印最终报告：

```
status: completed
continuity: 26f2f0a8-...
steps: 3
summary: task completed successfully. steps 3/200; ...
```

不带 `--task` 启动时，agent 进入 Idle 状态并干净退出。首次运行之后，
重启时会自动恢复未完成的连续性任务。

## Docker

```bash
docker build -t lemon-agent .
docker run --rm \
  -e AGENT_API_KEY=sk-... \
  -v lemon-data:/opt/lemon-agent \
  lemon-agent --task "用 Rust 实现斐波那契"
```

`deploy/docker-compose.yml` 提供了重启策略（`unless-stopped`）和日志轮转；
把 API 密钥放在它旁边的 `.env` 文件中：

```bash
cd deploy
echo "AGENT_API_KEY=sk-..." > .env
docker compose up -d
```

## systemd

```bash
sudo useradd -r -m -d /opt/lemon-agent lemon
sudo cp target/release/lemon-agent /opt/lemon-agent/
sudo cp -r scripts config.toml /opt/lemon-agent/
sudo chown -R lemon:lemon /opt/lemon-agent
sudo cp deploy/lemon-agent.service /etc/systemd/system/
echo "AGENT_API_KEY=sk-..." | sudo tee /etc/lemon-agent.env
sudo systemctl daemon-reload
sudo systemctl enable --now lemon-agent
```

该单元在失败时自动重启，降低权限，并把文件系统写入限制在
`/opt/lemon-agent` 内。

## 可观测性

日志是结构化的（`tracing`）。查看循环运行情况：

```bash
journalctl -u lemon-agent -f            # systemd
docker logs -f lemon-agent              # docker
```

每一步、工具调用、LLM 调用、错误、心跳和进化结果也都作为事件记录在
`agent.db` 中；查询方法见 [audit-and-recovery.md](audit-and-recovery.md)。
