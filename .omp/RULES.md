# Siskin 执行规则（OMP 主会话与子代理通用）

本文件每次请求都会随系统提示发送，保持简短。完整规范见 `AGENTS.md` 与
`docs/CONVENTIONS.md`；任务派发模板在 `docs/CONVENTIONS.md` §14（父代理使用，不下发）。

## 策略（用户确认后才动手；不自动提交）

1. **范围**：只改任务指定范围；不顺手重构、不改无关格式、不改依赖版本。
2. **边界**：遵守 crate 依赖方向；新 crate 必须先在 `docs/ARCHITECTURE.md` 出现。
3. **语言**：代码、注释、提交信息用英文；文档用中文；格式以 `rustfmt.toml` 为准。
4. **错误处理**：库 crate 不用 `anyhow`；非测试代码不用 `unwrap`/`expect`（不变量除外并写明）。
5. **安全**：凭据只走 `SecretStore`；日志禁止密码、令牌、完整邮箱、正文；邮件 HTML 必须净化。
6. **方案**：不写 v1/v2/MVP 式分层，不新增版本计划类文档。
7. **提交**：不入库 `target/`、`spike/data/`、生成物、数据库、密钥；**不自动 commit/push**，仅在大功能完成且用户明确同意后提交。
8. **协作**：未获用户确认，不开始实现、不改代码；先给方案，等确认。
9. **冲突**：任务要求与 `AGENTS.md` 冲突时停下提问，不自行取舍。

## 执行要求

- 开工前读 `docs/CONVENTIONS.md` 相关章节与 `docs/HANDOFF.md` 的进度矩阵；协议改动先看 `docs/SPIKE-REPORT.md` 是否已有实测结论。
- 改完实跑：`cargo fmt --all`；`cargo clippy --workspace --all-targets -- -D warnings`；`cargo test --workspace`。涉及 IMAP 的改动按 `docs/IMPLEMENTATION.md` §4 起 Dovecot 冒烟。
- 交付四段：改动（文件 → 行为）、验证（命令 → 输出摘要）、未验证与风险、文档影响。没跑过的写“未验证”，禁止用“应该可以”。

修改本文件时同步检查 `AGENTS.md` 是否仍一致。
