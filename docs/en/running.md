# Running Lemon Agent

Lemon Agent is a single static-friendly binary plus a `scripts/` directory and
a TOML config. This guide covers installation, configuration, and the two
supported deployment modes (Docker and systemd).

## Prerequisites

- Rust 1.97+ (only to build from source; releases ship the binary).
- `git` on the host or in the container: the agent stages and commits verified
  changes with it.
- An OpenAI-compatible API endpoint.

## Build

```bash
cargo build --release
# binary: target/release/lemon-agent
```

Verify the toolchain gates locally before shipping:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

## Configure

Copy `config.toml` and set the LLM endpoint. Secrets are never stored in the
file:

| Setting | Env var override | CLI flag |
|---------|------------------|----------|
| `llm.provider` | `AGENT_LLM_PROVIDER` | `--llm-provider` |
| `llm.api_key` | `AGENT_API_KEY` | `--api-key` |
| `llm.base_url` | `AGENT_LLM_BASE_URL` | `--llm-base-url` |
| `llm.model` | `AGENT_MODEL` | `--model` |
| `agent.work_dir` | `AGENT_WORK_DIR` | `--work-dir` |
| `agent.db_path` | `AGENT_DB_PATH` | `--db-path` |
| `logging.level` | `AGENT_LOG_LEVEL` | — |

Precedence: config file < environment < CLI flags. All invalid settings are
reported at startup before any work begins.

Minimal `config.toml`:

```toml
[agent]
work_dir = "./workspace"
scripts_dir = "./scripts"

[llm]
provider = "openai"          # openai | anthropic | gemini | custom
api_key = ""                 # use AGENT_API_KEY
base_url = "https://api.openai.com/v1"
model = "gpt-4"

[sandbox]
root_dir = "./workspace"
allowed_commands = ["git", "cargo", "rustc", "python3", "ls"]
```

## LLM providers

The LLM gateway supports four providers through a pluggable adapter:

| Provider | Protocol | Notes |
|----------|----------|-------|
| `openai` (default) | OpenAI chat completions | Also covers DeepSeek, Ollama, vLLM, and any OpenAI-compatible endpoint via `base_url`. |
| `anthropic` | Messages API | Uses `x-api-key` + `anthropic-version` headers; `max_output_tokens` is required and sent. |
| `gemini` | GenerateContent | Uses `x-goog-api-key`; the model is embedded in the request path. |
| `custom` | OpenAI-compatible with custom settings | For self-hosted gateways, proxies, and LLM aggregators. |

All providers support streaming, tool calls, retries with backoff, and
timeouts. The normalized message/tool model is identical across providers.

Example: Claude via Anthropic

```toml
[llm]
provider = "anthropic"
base_url = "https://api.anthropic.com"
model = "claude-3-5-sonnet-20241022"
max_output_tokens = 8192
```

Example: a custom OpenAI-compatible gateway (e.g. OneAPI-style proxy)

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

## Run

```bash
./target/release/lemon-agent \
  --config config.toml \
  --task "add rate limiting to this project and test it"
```

The agent plans, executes through `scripts/plan_and_execute.rhai`, verifies,
persists every event to `agent.db`, and prints a final report:

```
status: completed
continuity: 26f2f0a8-...
steps: 3
summary: task completed successfully. steps 3/200; ...
```

A run with no `--task` starts in the Idle state and exits cleanly. After the
first run, an unfinished continuity is resumed automatically on restart.

## Terminal UI

```bash
lemon-agent tui                        # run the agent with a live dashboard
lemon-agent tui --task "implement fibonacci"
lemon-agent tui --monitor              # watch an existing agent.db only
```

The TUI runs the agent as a daemon and lets you operate it from the terminal:

- **Dashboard**: current state, step count, budget usage, last error, final
  report, and a live event log.
- **Task submission**: type a task at the bottom and press Enter; tasks queue
  up and run one after another, each as its own continuity.
- **Continuities**: `c` opens the list of all continuities (steps, finished,
  started); Enter opens the full event log of the selected one.
- **Monitor mode**: `--monitor` attaches read-only to an existing `agent.db`
  (for example one written by a separate daemon process).

Keys: `Tab` focus input/log, `Enter` submit, `↑`/`↓`/`PgUp`/`PgDn` scroll,
`c` continuities, `d` detail, `Esc` back, `Ctrl+C` quit.

## Docker

```bash
docker build -t lemon-agent .
docker run --rm \
  -e AGENT_API_KEY=sk-... \
  -v lemon-data:/opt/lemon-agent \
  lemon-agent --task "implement fibonacci in Rust"
```

`deploy/docker-compose.yml` adds a restart policy (`unless-stopped`) and log
rotation; put the API key in a `.env` file next to it:

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

The unit restarts on failure, drops privileges, and confines filesystem
writes to `/opt/lemon-agent`.

## Observability

Logs are structured (`tracing`). Watch the loop with:

```bash
journalctl -u lemon-agent -f            # systemd
docker logs -f lemon-agent              # docker
```

Every step, tool call, LLM call, error, heartbeat, and evolution result is
also an event in `agent.db`; see [audit-and-recovery.md](audit-and-recovery.md)
for query recipes.
