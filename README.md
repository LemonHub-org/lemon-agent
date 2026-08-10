# Lemon Agent

An unattended, long-running autonomous programming agent built in Rust �?使用 Rust 构建的、可长期无人值守运行的自主编�?Agent�?
It plans, edits code, runs commands, tests, verifies, and evolves its own Rhai
strategy scripts within a safe sandbox. 它在安全沙箱内完成规划、代码修改�?命令执行、测试、验证，并自主进化自己的 Rhai 策略脚本�?
## Documentation / 文档

| English | 中文 |
|---------|------|
| [English overview](docs/en/README.md) | [中文总览](docs/zh-CN/README.md) |
| [Technical specification](docs/en/SPECS.md) | [技术规格](docs/zh-CN/SPECS.md) |
| [Project roadmap](docs/en/ROADMAP.md) | [项目路线图](docs/zh-CN/ROADMAP.md) |
| [Coding standards](docs/en/CODESTYLE.md) | [编码规范](docs/zh-CN/CODESTYLE.md) |
| [Running guide](docs/en/running.md) | [运行手册](docs/zh-CN/running.md) |
| [Audit and recovery](docs/en/audit-and-recovery.md) | [审计与恢复](docs/zh-CN/audit-and-recovery.md) |
| [Error codes](docs/en/error-codes.md) | [错误码与恢复策略](docs/zh-CN/error-codes.md) |
| [Versioning and migrations](docs/en/migrations.md) | [版本迁移策略](docs/zh-CN/migrations.md) |

## Quick start / 快速开�?
```bash
cargo build --release
export AGENT_API_KEY="sk-..."
./target/release/lemon-agent --config config.toml --task "implement fibonacci and test it"
```

Status: v0.2.0 released. 状态：v0.2.0 已发布�