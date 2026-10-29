# 仓库规则（Siskin）

本文件是仓库级约定，优先级高于用户级 AGENTS.md。改动本文件等于改团队规范，需在提交信息里说明原因。

## 开工前必读（按顺序）

1. `docs/HANDOFF.md` — 当前进度、环境、下一步、待决问题
2. `docs/ARCHITECTURE.md` — 11 条已定决策、数据模型、协议、安全基线
3. `docs/CONVENTIONS.md` — 代码风格与工程约定（本文件的展开版）
4. 涉及已实现模块时，再读 `docs/IMPLEMENTATION.md`

## 开发思路（不要偏离）

- **先验证再设计**：涉及外部协议或平台能力，先用最小工程实测；结论写进 `docs/SPIKE-REPORT.md` 或新报告。
- **协议与实现分离**：同步状态机、解析、加密在核心层；UI 不得直接说 IMAP，协议层不得依赖存储。
- **本地优先**：桌面可完全离线单机运行；Hub 是可选增强，不是前提。
- **单一方案，不分版本**：不写 v1/v2/MVP/后续迭代的代码分层；一次交付完整能力，内部只按依赖排序。
- **最小改动**：不顺手重构、不改无关格式、不扩大范围；代码因割接变废才删。
- **失败要显式**：错误分类（认证、限流、协议、数据），不吞异常，不伪造成功。
- **数据正确性优先于性能**：UIDVALIDITY、夏令时、Message-ID 去重这类问题先保证正确，再谈优化。

## 语言与风格

- 代码、标识符、注释、提交信息：**英文**。
- 文档、README、面向用户的说明：**中文**。
- 格式以 `rustfmt.toml` 为准，提交前必须 `cargo fmt --all`。
- 详细规则见 `docs/CONVENTIONS.md`，不允许各写一套。

## 必跑命令

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

若 PATH 中没有 cargo（本机安装时未改 PATH），使用 `~/.cargo/bin/cargo`。
本机网络访问 crates.io 需 `~/.cargo/config.toml` 中的镜像配置（不在仓库内）。

## 目录边界（违反即回退）

```
mail-core → mail-sync → mail-proto
         → mail-store → (sqlite)
         → mail-parse / mail-crypto / mail-translate
mail-ffi  → mail-core
mail-api  → mail-store
mail-hub  → mail-api, mail-core
```

- `mail-parse` 不做网络；`mail-store` 不依赖 UI 与协议类型；`mail-api` 不依赖 IMAP 类型。
- 新 crate 必须先在 `docs/ARCHITECTURE.md` 的边界图里出现。

## 错误与日志

- 每个库 crate 定义自己的 `XxxError`（`thiserror`）与 `pub type Result<T, E = XxxError>`。
- 库 crate 不使用 `anyhow`；`anyhow` 只允许出现在二进制、示例与测试。
- 非测试代码禁止 `unwrap()` / `expect()`，除非是不变量且写明原因。
- 日志用 `tracing`，字段结构化；**禁止**记录密码、令牌、完整邮箱地址、正文。

## 测试

- 测行为，不测接线：覆盖边界、状态迁移、优先级、错误路径。
- 不写“函数被调用”“字段被复制”“长度变大”这类无意义断言。
- 存储测试用 `Store::open_in_memory()`；需要真实服务器的测试必须由环境变量显式开启，默认跳过。
- 修 bug 先复现，再修，再留回归测试。

## 文档

- 行为或架构变化必须同步更新 `docs/`；仅改实现细节时更新 `docs/IMPLEMENTATION.md`。
- 不新增“版本计划”“里程碑编号”类文档；进度统一写在 `docs/HANDOFF.md`。
- 实测数据必须来自真实运行，未运行的检查写“未验证”。

## 协作节奏（用户明确要求，最高优先级）

- **未获用户确认，不开始新的实现任务**：先给出方案/计划与影响范围，等用户确认后再改代码。
- **不自动提交**：禁止自动执行 `git commit` / `git push`。只有“大功能完成”且用户明确同意时才提交。
- 小改动、文档与规则修订不单独提交，随下一次获准的提交一起进入。
- 提交前先给出改动清单与验证结果，等用户同意。

## 提交

- 提交信息英文祈使句：`Add ...`、`Fix ...`、`Refactor ...`，正文写“为什么”。
- 提交前跑完上面三条命令。
- 生成物不入库：`target/`、`spike/data/`、`spike/ffi-probe/gen*`、`*.db*`、编译出的基准程序。
- 未经当次明确授权，不执行 `git push` 到非本仓库远端。

## 安全红线

- 凭据只经 `mail-crypto::SecretStore`，绝不写入 SQLite、日志或错误信息。
- 邮件 HTML 必须经统一净化策略，禁止在 UI 层自行放宽白名单。
- 新增依赖需在 `docs/CONVENTIONS.md` 的依赖政策下评估许可与维护状态。

## 不要做

- 不要引入与 AGPL-3.0 不兼容的代码。
- 不要提交真实邮箱凭据、令牌、私钥。
- 不要为了“看起来整洁”批量重命名或重排现有代码。
- 不要用轮询替代 IDLE/CONDSTORE 已有能力；不要在没有实测的情况下声称某平台可行。
