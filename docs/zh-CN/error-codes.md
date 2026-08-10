# 错误码与恢复策略 (Error Codes and Recovery Strategy)

agent 暴露的每个失败都带有稳定错误码。下面的恢复策略与 SPECS.md
附录 C 一致，并由调度器、沙箱和 LLM 网关强制执行。

| 错误码 | 类别 | 含义 | 恢复策略 |
|------|-------|---------|-------------------|
| E001 | FileNotFound | 请求的文件不存在。 | `read_file`/`list_dir` 大声失败；`write_file`/`append_file` 会创建缺失的父目录。 |
| E002 | CommandTimeout | 沙箱命令超过时间限制。 | 在 `CommandOutput.timed_out` 中报告；可重试，子进程会被杀死。 |
| E003 | Llm | LLM 网关失败（HTTP 错误、畸形负载、流失败）。 | 瞬时失败（429、5xx、网络、超时）以指数退避重试；客户端错误不重试。 |
| E004 | Script | Rhai 脚本编译或运行失败。 | 触发进化引擎：生成候选脚本，编译，在隔离环境中验证，失败则回滚。 |
| E005 | BudgetExhausted | 达到步骤、输入 token、LLM 调用、工具调用或墙钟时间上限。 | 连续性安全终止，并持久化带用量信息的最终报告。 |
| E006 | CapabilityDenied | 调用方缺少所需的能力令牌。 | 在使用点拒绝，并作为 `ToolCall` 错误审计。 |
| E007 | PathViolation | 路径逃出沙箱根目录。 | 在任何文件系统访问之前拒绝；记录审计。 |
| E008 | Io | 文件系统操作失败。 | 携带路径传播；原子写入防止部分状态。 |
| E009 | Database | SQLite 事件库失败。 | WAL 模式加 5 秒 busy timeout；损坏或锁失败时大声报错。 |
| E010 | InvalidConfig | 配置无效。 | 启动时在任何工作开始前报告所有问题。 |
| E011 | InvalidInput | 外部输入无效（畸形计划、注入尝试）。 | 拒绝；计划重试一次，然后连续性失败。 |
| E012 | Timeout | 异步操作超过时间限制。 | 可重试；对每个阻塞操作都强制超时。 |
| E013 | RetryExhausted | 可重试操作在配置次数后仍失败。 | 附带尝试次数报告；由调用方决定恢复。 |
| E014 | Http | LLM 网关之外的网络请求失败。 | 携带根本原因传播。 |
| E015 | Json | 序列化或反序列化失败。 | 传播；持久化数据绝不静默重解释。 |
| E016 | AtomicWrite | 原子文件写入失败。 | 附带目标路径报告；临时文件会被清理。 |
| E017 | EvolutionRejected | 进化产生了被拒绝或无效的脚本。 | 恢复并重载上一个脚本版本。 |
| E018 | Internal | 不变量被破坏（事件日志缺口、状态中毒）。 | agent 大声停止；绝不从不一致的状态继续。 |

## 最终报告

`ContinuityFinished` 事件和打印出的报告始终包含：

- 连续性 ID，
- 终止状态（`completed`、`failed`、`budget_exhausted` 或 `idle`），
- 步骤数，以及
- 摘要；运行失败时包含错误码。
