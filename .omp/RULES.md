# Siskin 硬规则（OMP 子代理同样适用）

本文件是 `AGENTS.md` 的子集，只放必须始终生效的短约束。完整规范见 `AGENTS.md` 与
`docs/CONVENTIONS.md`，流程、交付格式与任务模板见 `SUBTASK-BRIEF.md`。
修改本文件时，必须同步检查 `AGENTS.md` 是否仍然一致。

1. 动手前先读 `SUBTASK-BRIEF.md` 与 `docs/CONVENTIONS.md` 的相关章节。
2. 只改任务指定范围；不顺手重构、不改无关格式、不改依赖版本。
3. 遵守 crate 依赖方向；新 crate 必须先在 `docs/ARCHITECTURE.md` 出现。
4. 代码、注释、提交信息用英文；文档用中文；提交前 `cargo fmt --all`。
5. 库 crate 不用 `anyhow`；非测试代码不用 `unwrap`/`expect`（不变量除外并写明原因）。
6. 凭据只走 `SecretStore`；日志禁止出现密码、令牌、完整邮箱地址、正文；邮件 HTML 必须净化。
7. 不写 v1/v2/MVP 式分层，不新增版本计划类文档。
8. 不提交 `target/`、`spike/data/`、生成物、数据库文件、任何密钥。
9. 未经当次明确授权，不执行 `git commit` / `git push`。
10. 改完必须实跑验证；没跑过的检查写“未验证”，不得写成已验证。
11. 任务要求与 `AGENTS.md` 冲突时停下提问，不自行取舍。
