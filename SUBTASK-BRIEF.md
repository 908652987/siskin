# 子任务入口（给 omp 子代理 / Codex 等执行者）

本文件只讲**流程**：开工前读什么、怎么验证、交付怎么写。
**策略与硬约束不在这里重复**：OMP 子代理会自动收到 `.omp/RULES.md` 与 `AGENTS.md` 的子集；
其他工具以仓库根目录 `AGENTS.md` 为准。

**冲突时优先级**：`AGENTS.md` > `docs/CONVENTIONS.md` > 本文件。

---

## 1. 开工前必读（按顺序）

1. `AGENTS.md`（仓库根目录，全部；OMP 子代理拿不到它，改以 `.omp/RULES.md` + 本文件为准）
2. `docs/CONVENTIONS.md` 中与本次改动相关的章节
3. `docs/HANDOFF.md` 的「进度矩阵」与「已知缺口」，确认你要做的东西没被做过
4. 涉及具体模块时读该模块源码；涉及外部协议时先看 `docs/SPIKE-REPORT.md` 是否已有实测结论

子代理不继承父会话上下文，任务描述必须自包含。任务文本与上述文件冲突时停下提问，不要猜。

---

## 2. 硬约束在哪

| 执行者 | 硬约束来源 |
|---|---|
| OMP 主会话 | `AGENTS.md` + `.omp/RULES.md` |
| OMP 子代理 | `.omp/RULES.md`（`AGENTS.md` 会被过滤，不会下发） |
| Codex / Claude Code | `AGENTS.md` |

执行中**不要**反复跑全量构建与测试；改完后按下节统一验证一次。

---

## 3. 验证要求

完成任务后必须实际运行改动路径，而不是只编译：

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

涉及 IMAP 的行为改动，还需按 `docs/IMPLEMENTATION.md` §4 起 Dovecot 做一次冒烟。
无法验证的部分，明确写“未验证”和原因，不得写成已验证。

---

## 4. 交付格式（回复必须包含）

```
# 改动
- 文件 → 行为变化（一句话一条）

# 验证
- 命令 → 关键输出摘要（真实运行过的）

# 未验证 / 风险
- 具体项 → 原因 → 需要什么条件

# 对文档的影响
- 需要更新的文档章节（若已更新，注明）
```

不要用“应该可以”“理论上”代替实跑结果。

---

## 5. 任务模板（父代理派发时直接粘贴）

```markdown
# Target
文件/模块：
非目标：

# Context
必读：AGENTS.md、docs/CONVENTIONS.md、docs/HANDOFF.md 相关段落
当前行为：
期望行为：

# Change
1. …
2. …

# Constraints
- 不改动：…
- 边界：…
- 依赖：不新增 / 允许新增（需说明理由）

# Acceptance
- 可观察结果：…
- 必须跑的命令：cargo fmt / clippy / test（以及 Dovecot 冒烟，若涉及协议）
- 不得出现：新增 warning、未使用代码、TODO 占位
```

---

## 6. 常见误判

| 情况 | 正确做法 |
|---|---|
| 发现无关的坏味道 | 记录到交付的「风险」里，不动手 |
| 测试需要真实服务器 | 用环境变量开启，默认跳过；不要改测试去适配环境 |
| 不确定某平台是否可行 | 标“未验证”，不要凭经验断言 |
| 想加一个小依赖图省事 | 先确认标准库/现有依赖不可行，再按依赖政策评估 |
