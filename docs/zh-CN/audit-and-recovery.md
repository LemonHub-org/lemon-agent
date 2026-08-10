# 审计与恢复 (Audit and Recovery)

agent 所做的一切都会作为不可变事件日志（`events` 表）追加到 `agent.db`
中，并周期性地写入状态快照（`snapshots` 表）。本指南说明如何审计一次
运行，以及崩溃后如何恢复。

## 审计

schema 稳定且记录在 `SPECS.md` 中。使用 `sqlite3` CLI 的常用查询：

```sql
-- 最新连续性任务的完整时间线（新的在前）：
SELECT seq, event_type, timestamp, payload
FROM events
WHERE continuity_id = (SELECT continuity_id FROM events
                       ORDER BY id DESC LIMIT 1)
ORDER BY seq;

-- 某个连续性中的所有工具调用：
SELECT seq, payload FROM events
WHERE continuity_id = 'CONTINUITY_ID' AND event_type = 'ToolCall';

-- 失败与进化结果：
SELECT seq, event_type, payload FROM events
WHERE continuity_id = 'CONTINUITY_ID'
  AND event_type IN ('Error', 'EvolutionAttempt', 'EvolutionResult');

-- 未完成的连续性（可恢复的候选）：
SELECT DISTINCT continuity_id FROM events e
WHERE NOT EXISTS (SELECT 1 FROM events f
                  WHERE f.continuity_id = e.continuity_id
                    AND f.event_type = 'ContinuityFinished');
```

快照状态是序列化的 agent 上下文：状态机位置、计划、对话消息和预算用量。

## 恢复

启动时，agent 会：

1. 找到最新的未完成连续性。
2. 加载其最新快照。
3. 验证从快照起的日志事件没有缺口（`verify_continuity`）。
4. 重放事件以重新统计预算用量和进化尝试次数。
5. 从持久化的状态机位置继续执行。

两种特殊情况：

- **首个快照前崩溃**：任务从 `ContinuityStarted` 事件的初始提示词重新开始。
- **损坏或更新的数据库**：agent 拒绝打开并带清晰错误退出（见
  `migrations.md`）。它绝不会从不一致的状态静默继续。

## 保证

- 每个外部副作用（文件、进程、Git、LLM）都是审计事件。
- 每个连续性的序列号单调递增；出现缺口时恢复阶段会大声失败。
- 进化候选在隔离沙箱中验证，失败时回滚；成功的尝试不会残留 `.bak`
  文件。
- API 密钥和机密绝不会出现在事件、日志或提示词预览中。
