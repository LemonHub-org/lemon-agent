# 版本与迁移 (Versioning and Migrations)

两个持久化契约需要版本纪律：SQLite schema 和 Rhai 脚本接口。

## SQLite schema

schema 版本存储在 SQLite 的 `PRAGMA user_version` 中（当前为 `1`）。打开
时，agent 会：

- 拒绝 `user_version` 大于二进制 `SCHEMA_VERSION` 的数据库（降级会静默
  丢弃数据），并且
- 通过运行幂等的 schema 批处理并提升 `user_version` 来升级旧数据库。

添加迁移的步骤：

1. 提升 `src/kernel/event_store.rs` 中的 `SCHEMA_VERSION`。
2. 在 `migrate()` 的迁移路径中加入 DDL（该批处理是幂等的，因此
   `CREATE TABLE IF NOT EXISTS` 和追加性语句是安全的）。
3. 为旧 → 新升级添加测试（参见 `older_schema_version_is_upgraded`）。

## 脚本接口

策略脚本契约是 `execute_plan(plan)` 入口点，加上 `SPECS.md` 中记录的
工具名称：

- 脚本必须导出 `fn execute_plan(plan)`；引擎拒绝加载任何其他形式。
- 可选的自测入口是 `test_<script_name>()`；进化要求候选脚本具备它，
  才能替换线上脚本。
- 脚本只能调用已注册的工具函数；其他调用会导致编译失败。

接口变更对以下内容属于破坏性变更：

- 仓库内置的脚本（`scripts/plan_and_execute.rhai`），必须在同一发布中
  更新，以及
- `src/evolution/mod.rs` 中的进化提示词，必须记录新契约，让候选脚本
  符合要求。

任何接口变更后，运行 `cargo test --test script_engine --test evolution`
来验证脚本兼容性。
