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
api_key = ""                 # use AGENT_API_KEY
base_url = "https://api.openai.com/v1"
model = "gpt-4"

[sandbox]
root_dir = "./workspace"
allowed_commands = ["git", "cargo", "rustc", "python3", "ls"]
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
