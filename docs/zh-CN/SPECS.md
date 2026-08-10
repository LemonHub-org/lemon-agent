# SPECS.md �?Lemon Agent 技术规格说明书

> **版本**: 0.1.0  
> **目标**: 构建一个用 Rust 从零开发的、支持无人值守长时自主编程与自主进化的 AI Agent�? 
> **核心原则**: 极简依赖、高性能、安全隔离、热进化�?

---

## 1. 项目概述

### 1.1 目标
创建一个长期运行的守护进程，它能够�?
- 接收编程任务（通过初始指令或外部接口）
- 在无人工干预下，持续执行代码编写、测试、调试、重构等编程活动
- 根据执行反馈自主优化其行为逻辑（进化）
- 提供完整的审计日志和恢复能力

### 1.2 范围
- **语言**: Rust (稳定�?2024+)
- **核心能力**: 文件读写、命令执行、代码搜索、版本控制（Git）、LLM 调用
- **进化范围**: 仅限于脚本层（Rhai 脚本）的热更新，不涉�?Rust 内核的自我修�?
- **运行环境**: 跨平�?
### 1.3 非目�?
- 不提供图形界面或交互式终端（可通过日志监控�?
- 不支持分布式集群（单机设计）

---

## 2. 系统架构

### 2.1 分层模型

```
┌─────────────────────────────────────────�?
�?          外部触发（初始任�?API�?       �?
└─────────────────┬───────────────────────�?
                  �?
┌─────────────────────────────────────────�?
�?        调度�?(Scheduler)               �? �?Rust 核心，不可变
�? - 主循�?(while-true)                  �?
�? - 状态机（IDLE, PLANNING, EXECUTING,   �?
�?           EVALUATING, EVOLVING)        �?
�? - 上下文管�?(滑动窗口)                �?
�? - 预算控制 (步数/Token/调用次数)       �?
└──────────────┬──────────────────────────�?
               �?
               �?
┌─────────────────────────────────────────�?
�?        能力�?(Capability Layer)        �? �?Rust 核心，安全边�?
�? - 权限令牌 (Capability Tokens)         �?
�? - 沙箱执行�?(Sandbox Executor)        �?
�? - 工具函数 (Tools: fs, process, git)   �?
└──────────────┬──────────────────────────�?
               �?
               �?
┌─────────────────────────────────────────�?
�?        脚本引擎 (Script Engine)         �? �?Rhai 运行时，可进�?
�? - 动态加�?scripts/*.rhai              �?
�? - 暴露 Rust 工具�?Rhai 函数           �?
�? - 热重�?(hot-reload)                  �?
└──────────────┬──────────────────────────�?
               �?
               �?
┌─────────────────────────────────────────�?
�?       进化引擎 (Evolution Engine)       �? �?部分在脚本，部分在Rust
�? - 错误捕获 �?生成改进脚本              �?
�? - 脚本替换与验�?                      �?
└─────────────────────────────────────────�?
```

### 2.2 数据存储
- **SQLite**: 单一数据库文�?`agent.db`，用于事件溯源和状态快照�?
- **文件系统**: 项目代码存储在工作目录；脚本存储�?`scripts/` 子目录�?

---

## 3. 核心组件详解

### 3.1 事件溯源存储 (Event Store)

**文件**: `src/kernel/event_store.rs`

使用 SQLite 存储不可变事件日志，并支持快照以加速恢复�?

#### 表结�?

```sql
-- 事件�?
CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    continuity_id TEXT NOT NULL,          -- 当前连续性会话ID
    seq INTEGER NOT NULL,                 -- 序列号，单调递增
    timestamp INTEGER NOT NULL,           -- Unix 毫秒
    event_type TEXT NOT NULL,             -- 事件类型，如 "StepStarted", "ToolCall", "LLMResponse", "Error", "Heartbeat"
    payload JSON NOT NULL,                -- 事件具体数据
    UNIQUE(continuity_id, seq)
);

-- 快照�?
CREATE TABLE snapshots (
    continuity_id TEXT PRIMARY KEY,
    state JSON NOT NULL,                  -- 序列化的完整状态（包括上下文、预算等�?
    seq INTEGER NOT NULL,                 -- 快照对应的事件序列号
    timestamp INTEGER NOT NULL
);

-- 索引
CREATE INDEX idx_events_continuity ON events(continuity_id, seq);
```

#### 事件类型定义 (Rust enum)

```rust
pub enum EventType {
    ContinuityStarted { initial_prompt: String },
    StepStarted { step_num: usize },
    LLMRequest { prompt_preview: String, tools: Vec<String> },
    LLMResponse { content: String, tool_calls: Vec<ToolCall> },
    ToolCall { tool_name: String, args: Value, result: Result<Value, String> },
    Error { error: String, recoverable: bool },
    Heartbeat { steps_since_last: usize },
    EvolutionAttempt { script_path: String, old_hash: String, new_hash: String },
    EvolutionResult { success: bool, reason: String },
    StepFinished { step_num: usize },
    ContinuityFinished { status: String, summary: String },
}
```

**恢复流程**:
1. �?`snapshots` 读取最新快照（�?`seq` 降序）�?
2. �?`events` 中读取该 `seq` 之后的所有事件�?
3. 按顺序重放事件，重建内存状态�?

### 3.2 调度�?(Scheduler)

**文件**: `src/scheduler/mod.rs`, `src/scheduler/loop.rs`

主循环是一个有限状态机，每个状态都通过事件存储持久化�?

#### 状态定�?

```rust
pub enum AgentState {
    Idle,                     // 等待新任�?
    Planning,                 // 生成任务计划（调用LLM�?
    Executing,                // 执行工具调用或脚�?
    Evaluating,               // 验证执行结果（如运行测试�?
    Evolving,                 // 根据失败尝试进化
    Terminated,               // 任务结束或预算耗尽
}
```

#### 主循环伪代码

```rust
async fn run_loop(mut ctx: Context) -> Result<()> {
    while let Some(state) = ctx.current_state {
        budget_check(&ctx)?;                      // 检查预�?
        let (next_state, events) = match state {
            Idle => handle_idle(&mut ctx).await,
            Planning => handle_planning(&mut ctx).await,
            Executing => handle_executing(&mut ctx).await,
            Evaluating => handle_evaluating(&mut ctx).await,
            Evolving => handle_evolving(&mut ctx).await,
            Terminated => break,
        };
        append_events(events);                    // 持久化所有事�?
        ctx = apply_state_transition(ctx, next_state);
        create_snapshot_if_needed(&ctx);          // 每N步或N分钟
        sleep(100ms).await;                       // 防止CPU空转
    }
    Ok(())
}
```

### 3.3 上下文管�?(Context Manager)

**文件**: `src/scheduler/context.rs`

管理 LLM 的对话上下文，采�?*滑动窗口**保留最�?`K` 条消息，并结合摘要压缩�?

- 数据结构: `Vec<Message>`，每条消息包�?`role` (system/user/assistant/tool) �?`content`�?
- 限制: �?token 数不得超�?`MAX_CONTEXT_TOKENS` (�?128k)�?
- 压缩策略: �?token 数超过阈值时，调�?LLM 生成之前对话的摘要，替换为一�?system 消息�?

### 3.4 预算控制 (Budget Controller)

**文件**: `src/scheduler/budget.rs`

硬限制，防止无限循环和费用失控�?

```rust
pub struct Budget {
    pub max_steps: usize,           // 默认�?200
    pub max_input_tokens: usize,    // 默认�?100_000
    pub max_llm_calls: usize,       // 默认�?50
    pub max_tool_calls: usize,      // 默认�?200
    pub max_wall_clock_secs: u64,   // 默认�?86400 (24小时)
}
```

检查点: 每个步骤开始前调用 `check_budget()`，如果超限则触发 `Terminated` 状态并记录原因�?

### 3.5 能力与沙�?(Capability & Sandbox)

**文件**: `src/kernel/capability.rs`, `src/kernel/sandbox.rs`

#### 权限令牌

每个工具调用都需要提供有效的权限令牌。令牌结�?

```rust
pub struct Capability {
    pub resource: Resource,        // �?"fs:/project", "process:allowed_commands"
    pub permissions: Permissions,  // �?�?执行
    pub expires_at: Option<Instant>,
}
```

沙箱执行器会校验调用者是否持有相应令牌。默认只有主循环持有完整令牌，子任务可获取受限令牌�?

#### 执行沙箱

所有外部操作（文件、进程、网络）都通过 `Sandbox` 结构执行�?

```rust
pub struct Sandbox {
    root_dir: PathBuf,            // 允许访问的根目录（项目目录）
    allowed_commands: HashSet<String>, // �?["git", "cargo", "python"]
}
impl Sandbox {
    pub async fn read_file(&self, path: &Path) -> Result<String>;
    pub async fn write_file(&self, path: &Path, content: &str) -> Result<()>;
    pub async fn exec_command(&self, cmd: &str, args: &[String]) -> Result<CommandOutput>;
    // ... 其他工具
}
```

所有操作都会记录审计日志（通过事件 `ToolCall`）�?

### 3.6 LLM 网关 (LLM Gateway)

**文件**: `src/llm/client.rs`, `src/llm/provider.rs`

封装 HTTP 请求，通过可插拔适配器支持多提供商：`openai`（OpenAI 兼容
chat completions，同样通过 `base_url` 覆盖 DeepSeek、Ollama、vLLM 及任�?
兼容网关）、`anthropic`（Messages API）、`gemini`（GenerateContent），以及
`custom`（可配置路径、认证头和附加请求头�?OpenAI 兼容端点）�?

- 使用 `reqwest` 异步客户端，带重试机制（指数退避，最�?次）�?
- 支持流式响应（可选），以便实时输出�?
- 支持工具调用（Function Calling�? �?Rust 工具转换为各提供商对应的 schema�?
- 各提供商拥有各自的线上格式；消息、工具和响应被规范化为统一�?
  提供商无关模型，因此调度器、脚本和进化引擎与具体提供商解耦�?

```rust
pub struct LLMClient {
    api_key: String,
    base_url: String,
    model: String,
    client: reqwest::Client,
    provider: Box<dyn LlmProvider>,
}

pub trait LlmProvider: Debug + Send + Sync {
    fn chat_path(&self, model: &str, stream: bool) -> String;
    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)>;
    fn build_body(&self, model: &str, max_output_tokens: u64, messages: &[Message],
                  tools: &[ToolDefinition], temperature: f64, stream: bool) -> Result<Value>;
    fn parse_completion(&self, body: &Value, model: &str) -> Result<LLMResponse>;
    fn stream_parser(&self) -> Box<dyn StreamParser>;
}
```

### 3.7 脚本引擎 (Script Engine)

**文件**: `src/evolution/script_engine.rs`

基于 `Rhai` 嵌入式脚本语言，动态加�?`scripts/` 目录下的 `.rhai` 文件�?

**暴露给脚本的 Rust 函数**（通过 `register_fn`）：

```rust
// 所有工具函数都包装�?Rhai 可调用的
engine.register_async_fn("read_file", |path: String| async { /* ... */ });
engine.register_async_fn("write_file", |path, content| async { /* ... */ });
engine.register_async_fn("exec", |cmd, args| async { /* ... */ });
engine.register_async_fn("llm_query", |prompt| async { /* ... */ });
engine.register_fn("log", |msg: String| { tracing::info!("{}", msg) });
```

**脚本加载**:

- 启动时扫�?`scripts/` 下所�?`.rhai` 文件，编译为 `AST` 并缓存�?
- 热重�? 使用 `notify` 库监听文件变化，自动重新编译更新�?

**脚本约定**:

- 每个脚本文件应导出一个主函数，命名如 `execute_plan(plan)`�?
- 脚本通过返回 `Result<String, String>` 表示成功/失败�?

### 3.8 进化引擎 (Evolution Engine)

**文件**: `src/evolution/mod.rs`

处理脚本级别的自我改进�?

#### 进化触发条件

- 执行脚本后返回错误（`Err`�?
- 工具调用返回失败（如测试失败�?
- LLM 评估认为当前策略不佳

#### 进化流程

1. **捕获失败上下�?*: 收集错误消息、当前脚本源码、最近N条对话消息、相关事件�?
2. **生成进化提示**: 构建一个系统提示，要求 LLM 分析失败原因并提供改进后的脚本代码�?
3. **调用 LLM**: 使用 `llm_query` 获取新脚本内容�?
4. **备份与原子写�?*:
   - 将原脚本重命名为 `.bak`
   - 写入新内容到原文件（`write_file` 原子操作�?
   - 触发脚本引擎重载该文�?
5. **验证**: 使用新脚本重新执行刚才失败的任务（或一组测试）�?
6. **回滚**: 如果验证失败，恢复备份文件并记录失败�?
7. **记录事件**: 生成 `EvolutionAttempt` �?`EvolutionResult` 事件�?

**安全措施**:
- 验证新脚本不包含危险操作（如删除文件），通过静态分析（可选）�?
- 限制进化尝试次数（如每个任务最�?次）�?

---

## 4. 工具清单 (Tools)

所有工具都通过 `Sandbox` 实现，并�?Rhai 函数形式暴露�?

| 工具名称 | Rhai 签名 | 安全限制 | 备注 |
|---------|----------|---------|------|
| `read_file` | `fn read_file(path: String) -> Result<String>` | 路径必须�?`root_dir` 内；文件大小<10MB | 异步 |
| `write_file` | `fn write_file(path: String, content: String) -> Result<()>` | 路径限制；原子写入；文件大小<10MB | 异步 |
| `append_file` | `fn append_file(path: String, content: String) -> Result<()>` | 同上 | 异步 |
| `list_dir` | `fn list_dir(path: String) -> Result<Array>` | 路径限制 | 异步 |
| `search_code` | `fn search_code(query: String, path: String) -> Result<Array>` | 使用 `ignore` 库，限于文本文件 | 异步 |
| `exec_command` | `fn exec_command(cmd: String, args: Array) -> Result<Object>` | 命令白名单；禁止交互；超�?20�?| 异步 |
| `git_add_commit` | `fn git_add_commit(message: String) -> Result<String>` | 必须为Git仓库；限制分�?| 异步 |
| `llm_query` | `fn llm_query(prompt: String) -> Result<String>` | 受预算限�?| 异步 |
| `sleep` | `fn sleep(ms: int) -> Result<()>` | 用于延迟 | 异步 |
| `log_debug` | `fn log_debug(msg: String)` | 仅日�?| 同步 |

**注意**: 所有工具执行结果都将被记录为事�?`ToolCall`，方便审计�?

---

## 5. 数据流与生命周期

以一次典型任务执行为例：

1. **启动**: Agent 加载快照或从 `Idle` 状态开始，等待输入任务（可通过命令行参数或 FIFO 文件）�?
2. **Planning**: 调用 LLM，传入系统指令（包括可用工具）和用户需求，获取一个计划（步骤列表）�?
3. **Executing**: 依次执行计划中的每个步骤。每一步可能调用一个或多个工具，或调用脚本函数�?
4. **Evaluating**: 执行完计划后，运行验证脚本（如测试套件）。若失败，则触发进化�?
5. **Evolving**: 如果验证失败，进入进化流程，修改相关脚本，然后重复执行步�?-4�?
6. **Termination**: 验证通过或预算耗尽，生成最终报告，保存快照，进�?`Idle` 等待新任务�?

所有状态转换都通过事件存储持久化，确保崩溃恢复�?

---

## 6. 安全与可靠性设�?

### 6.1 心跳与看门狗
- **内部**: 每执行一个步骤，写入 `Heartbeat` 事件�?
- **外部**: 使用 `tokio::time::timeout` 包裹每个异步操作（如 LLM 调用、工具执行），超时后视为失败并记录错误�?
- **系统�?*: 可通过 systemd �?Docker 重启策略监控进程存活�?

### 6.2 恢复机制
- 启动时自动从 SQLite 恢复最近快�?事件�?
- 若连续�?ID 存在且未完成，则继续执行；否则创建新连续性�?

### 6.3 权限最小化
- 默认只授予基本文件读权限�?
- 写文件、执行命令需通过能力校验�?

### 6.4 输入验证
- 所有外部输入（如文件路径、命令参数）都进行净化，防止注入�?

---

## 7. 进化机制详细流程

**触发**: 脚本执行返回 `Err` 或工具调用返�?`Err`（可配置）�?

**步骤**:
1. **收集上下�?*:
   - 当前连续�?ID, 步骤序号
   - 错误信息（error 字符串）
   - 失败的脚本源码（从文件读取）
   - 最�?10 条对话消�?
   - 最近的 5 个工具调用结�?
2. **生成进化提示**: 构建结构化提示，要求 LLM 修复脚本�?
3. **调用 LLM**: 使用 `llm_query` 获取新脚本源码�?
4. **备份**: �?`scripts/<script_name>.rhai` 重命名为 `<script_name>.rhai.bak`�?
5. **写入新脚�?*: 原子写入新内容�?
6. **重载**: 通知脚本引擎重新编译该脚本�?
7. **验证**: 使用 `sandbox` 调用新脚本的一个测试入口（�?`test_<script_name>()`）或重跑失败步骤�?
8. **结果处理**:
   - 成功：保留新脚本，记�?`EvolutionResult{success: true}`�?
   - 失败：恢复备份，记录 `EvolutionResult{success: false, reason}`，并可能降低该脚本的“信任分数”�?
9. **限制**: 每个连续性最多进�?`MAX_EVOLUTION_ATTEMPTS` (默认5) 次进化尝试�?

---

## 8. 配置与运行时参数

配置文件使用 TOML 格式 (`config.toml`)，可被命令行参数覆盖�?

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
provider = "openai"
api_key = "your-api-key"
base_url = "https://api.openai.com/v1"
model = "gpt-4"
temperature = 0.7
max_output_tokens = 4096

[sandbox]
root_dir = "./workspace"
allowed_commands = ["git", "cargo", "python3", "rustc", "ls"]

[logging]
level = "info"
file = "agent.log"
```

环境变量可覆盖：`AGENT_API_KEY`, `AGENT_LLM_BASE_URL` 等�?

---

## 9. 构建与部�?

### 9.1 编译
```bash
cargo build --release
```

### 9.2 运行
```bash
./target/release/agent --config config.toml --task "Write a Rust function to compute fibonacci" 
```

### 9.3 Docker 镜像
提供 Dockerfile，使�?`rust:alpine` 构建，复制二进制和脚本目录�?

### 9.4 日志监控
使用 `tracing` 输出结构化日志，可集�?`tracing-subscriber` 写入文件�?stdout�?

---

## 10. 已知限制与未来扩�?

### 10.1 当前限制
- 仅支持单线程异步执行�?
- 进化仅限于脚本层，无法修�?Rust 内核�?
- 没有多项目隔离，所有任务共享工作目录�?
- 依赖 LLM 的稳定性，可能因API限流或错误而中断�?

### 10.2 未来计划
- **分布式执�?*: 使用 `tokio` 多线程，支持并行子任务�?
- **更丰富的工具**: 支持数据库查询、网络请求、Docker操作�?
- **深度学习辅助**: 基于历史事件微调提示或选择最优脚本�?
- **自我测试**: 自动生成测试套件并运行�?
- **Rust 源码级进化（探索性）**: 在完全受控的实验中，�?Agent 生成 Rust 代码并通过热加载动态库（`libloading`）实现部分功能更新�?

---

## 附录 A：示�?Rhai 脚本

`scripts/plan_and_execute.rhai`:

```rhai
// 接收计划描述，执行一系列步骤
fn execute_plan(plan) {
    let steps = plan.steps;
    for step in steps {
        log_debug("Executing step: " + step.name);
        match step.tool {
            "read_file" => {
                let content = read_file(step.args.path);
                if content.is_err() {
                    return Err("Read failed: " + content.err());
                }
                // 将内容存入上下文
            },
            "write_file" => {
                let result = write_file(step.args.path, step.args.content);
                if result.is_err() {
                    return Err("Write failed: " + result.err());
                }
            },
            "llm_query" => {
                let response = llm_query(step.args.prompt);
                if response.is_err() {
                    return Err("LLM error: " + response.err());
                }
                // 处理响应
            },
            // ... 其他工具
        }
    }
    Ok("Plan executed successfully")
}
```

---

## 附录 B：事件存储示�?

```json
// 一个事件示�?(StepStarted)
{
  "type": "StepStarted",
  "step_num": 12,
  "timestamp": 1712345678901
}
```

---

## 附录 C：错误码与恢复策�?

| 错误�?| 描述 | 恢复策略 |
|-------|------|---------|
| E001 | 文件不存�?| 尝试创建或跳�?|
| E002 | 命令执行超时 | 增加超时或重�?|
| E003 | LLM 返回错误 | 重试或降级模�?|
| E004 | 脚本语法错误 | 触发进化修复脚本 |
| E005 | 预算耗尽 | 终止任务，生成报�?|
