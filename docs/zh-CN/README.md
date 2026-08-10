# Lemon Agent

Lemon Agent 是一个使用 Rust 构建的、可长期无人值守运行的自主编程 Agent。它能够接收一个编程目标，持续完成规划、代码修改、命令执行、测试、调试和结果评估，并在安全边界内改进自己的 Rhai 执行脚本。

> **状态：v0.1.0 已发布。** 完整的单机闭环已经可用：规划、脚本驱动执行、
> 验证、事件溯源持久化与崩溃恢复、可热重载的 Rhai 策略，以及带回滚的
> 受控自我进化。阶段记录见 [ROADMAP.md](./ROADMAP.md)，部署方式见
> [running.md](./running.md)。

## 快速开始

```bash
cargo build --release
export AGENT_API_KEY="sk-..."
./target/release/lemon-agent --config config.toml --task "实现斐波那契并测试"
# 输出: status: completed / continuity: <id> / steps: N / summary: ...
```

提供 Docker（`docker build -t lemon-agent .`）与 systemd
（`deploy/lemon-agent.service`）两种部署方式，详见
[running.md](running.md)。

## 完成后的 Lemon Agent

Lemon Agent 将作为单机守护进程运行。用户只需提供工作目录、配置和初始任务，它便会自主推进任务，直到验证通过、达到预算上限或遇到无法安全恢复的问题。

一次典型运行将类似：

```bash
./target/release/agent \
  --config config.toml \
  --task "为这个 Rust 项目实现速率限制，并补充测试"
```

运行过程中，Agent 将：

1. 理解任务并生成可执行计划。
2. 在受限工作目录内搜索和读取代码。
3. 调用经过授权的文件、进程、Git 和 LLM 工具。
4. 修改代码并运行格式化、编译和测试命令。
5. 根据验证结果继续修复、重构或结束任务。
6. 持久化每一步操作，以便审计和崩溃恢复。
7. 必要时改进 Rhai 策略脚本，并在验证失败时自动回滚。

## 核心能力

- **自主任务循环**：通过 `Planning → Executing → Evaluating → Evolving` 状态机持续推进任务。
- **代码操作**：安全地读取、写入、追加、列出和搜索工作目录中的文件。
- **命令与测试**：执行白名单中的非交互式命令，例如 `git`、`cargo`、`rustc` 和 `python3`。
- **版本控制**：在受限分支和仓库范围内暂存并提交已验证的变更。
- **LLM 工具调用**：连接 OpenAI 兼容 API，支持结构化工具定义、重试和可选流式响应。
- **上下文压缩**：使用滑动窗口与摘要控制长时间会话的上下文大小。
- **预算控制**：限制步骤数、token、LLM 调用、工具调用和总运行时间，避免失控循环与费用超支。
- **事件溯源**：将任务、步骤、工具、LLM、错误、心跳和进化结果写入 SQLite。
- **崩溃恢复**：从最近快照和后续事件恢复状态，继续未完成的连续任务。
- **脚本热重载**：动态加载 `scripts/*.rhai`，无需重启即可更新执行策略。
- **受控进化**：根据失败上下文生成候选策略脚本，经过编译和验证后才允许替换。

## 系统形态

```text
任务输入 / 外部 API
        │
        ▼
Rust 调度器 ── 状态机、上下文、预算、恢复
        │
        ▼
能力与沙箱层 ── 权限令牌、路径隔离、命令白名单、审计
        │
        ▼
Rhai 脚本引擎 ── 可热重载的执行策略
        │
        ▼
进化引擎 ── 失败分析、候选生成、验证与回滚

SQLite 事件库贯穿所有层，保存事件和状态快照。
```

Rust 内核负责不可绕过的调度、安全、持久化和预算约束；Rhai 层负责可替换的高层执行策略。Agent 可以进化脚本行为，但不能自行修改 Rust 安全内核。

## 安全边界

自主运行不意味着无限权限。系统默认遵循最小权限原则：

- 所有文件访问都限制在配置的 `root_dir` 内，并阻止目录穿越。
- 写文件、执行命令和调用外部能力前必须通过能力令牌校验。
- 外部命令必须位于白名单中，禁止交互，并受超时限制。
- 每个外部副作用都会形成可查询的审计事件。
- API 密钥等敏感配置不会写入普通日志或 LLM 上下文预览。
- 每个异步操作均有超时，每个任务均受硬预算限制。
- 新生成的 Rhai 脚本必须先编译和验证；失败时恢复上一版本。
- 每个连续任务的进化次数有限，默认最多尝试 5 次。

## 可恢复、可审计的运行体验

Lemon Agent 使用 `agent.db` 保存不可变事件日志和周期性状态快照。即使进程被终止，重新启动后也能够：

- 找到最近一次未结束的连续任务。
- 从快照恢复上下文、状态和预算使用量。
- 重放快照之后的事件，重建一致的内存状态。
- 从安全的步骤边界继续执行。

用户可以通过结构化日志观察当前状态、步骤、工具调用、资源消耗和最终结果，而不需要图形界面或交互式终端。

## 项目结构

```text
.
├── src/
│   ├── kernel/
│   │   ├── capability.rs
│   │   ├── event_store.rs
│   │   └── sandbox.rs
│   ├── scheduler/
│   │   ├── budget.rs
│   │   ├── context.rs
│   │   ├── loop_runner.rs
│   │   ├── mod.rs
│   │   └── plan.rs
│   ├── llm/
│   │   └── client.rs
│   ├── evolution/
│   │   ├── mod.rs
│   │   └── script_engine.rs
│   └── main.rs
├── scripts/
│   └── plan_and_execute.rhai
├── docs/
│   ├── en/          # 英文文档
│   └── zh-CN/       # 中文文档
├── deploy/          # systemd 与 Docker Compose 示例
├── Dockerfile
├── config.toml
├── agent.db
└── README.md        # 双语入口
```

其中 `workspace/` 是 Agent 被允许操作的项目目录，`scripts/` 保存可热更新的行为策略，`agent.db` 保存事件与快照。

## 配置示例

```toml
[agent]
work_dir = "./workspace"
scripts_dir = "./scripts"
max_steps = 200
max_input_tokens = 100000
max_llm_calls = 50
max_evolution_attempts = 5
heartbeat_interval_secs = 60
snapshot_interval_steps = 10

[llm]
base_url = "https://api.openai.com/v1"
model = "gpt-4"
temperature = 0.7

[sandbox]
root_dir = "./workspace"
allowed_commands = ["git", "cargo", "python3", "rustc"]

[logging]
level = "info"
file = "agent.log"
```

LLM 密钥将优先通过 `AGENT_API_KEY` 等环境变量提供，避免直接提交到配置文件。

## v0.1.0 的完成标准

首个稳定版本将能够：

- 在 Linux、Windows 和 macOS 上构建并运行。
- 在沙箱内独立完成一个小型真实代码任务并通过测试。
- 对每个步骤和外部副作用提供完整审计记录。
- 在进程异常退出后恢复并继续未完成任务。
- 在预算耗尽、外部超时或不可恢复错误发生时安全停止并生成报告。
- 热重载 Rhai 策略脚本，并可靠验证或回滚自主改进。
- 通过安全审查、故障注入测试和至少 24 小时稳定性测试。

> v0.1.0 验收状态：全部项目结构与 CI 门禁（fmt / clippy / test）通过；沙箱
> 端到端任务、崩溃恢复、预算边界、脚本热重载、进化修复与回滚、稳定性
> 周期测试均已有自动化覆盖。24 小时浸泡测试作为 `#[ignore]` 测试提供
> （`cargo test --test stability -- --ignored`）。

## 明确的非目标

v0.1.0 不计划提供：

- 图形界面或交互式终端。
- 分布式集群和并行多 Agent 调度。
- 多项目共享环境中的强隔离。
- Rust 内核源码的自主修改。
- 不受限制的网络、Shell 或宿主机访问。

这些限制让首个版本专注于一个更重要的目标：构建可靠、安全、可恢复、可审计的单机自主编程闭环。

## 文档

- [技术规格](./SPECS.md)
- [项目路线图](./ROADMAP.md)
- [编码规范](./CODESTYLE.md)
- [运行手册](./running.md)
- [审计与恢复](./audit-and-recovery.md)
- [错误码与恢复策略](./error-codes.md)
- [版本迁移策略](./migrations.md)
