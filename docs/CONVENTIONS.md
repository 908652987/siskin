# 代码与工程约定

本文件是 `AGENTS.md` 的展开版。目的是让不同时间、不同人（或不同模型）写出的代码看起来像同一个人写的。**改动这里的规则需要单独提交并说明理由。**

---

## 1. 格式基线

| 项 | 约定 |
|---|---|
| 格式化 | `rustfmt`，配置见 `rustfmt.toml`（最大宽度 100，Unix 换行，导入排序） |
| Lint | workspace 统一在 `Cargo.toml` 的 `[workspace.lints]`；CI 用 `-D warnings` |
| 缩进 | 4 空格，不用 Tab（`.editorconfig` 强制） |
| 换行 | 文件末尾保留一个换行 |
| 导入顺序 | `std` → 第三方 → 本 crate（rustfmt 默认分组） |

提交前必须：

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

---

## 2. 命名

| 对象 | 规则 | 例子 |
|---|---|---|
| crate | `mail-<层>`，小写连字符 | `mail-store`、`mail-proto` |
| 类型 / trait / 枚举 | PascalCase | `ImapSession`、`SecretStore` |
| 函数 / 变量 / 模块 | snake_case | `fetch_range`、`apply_fetch` |
| 常量 | SCREAMING_SNAKE_CASE | `FLAG_RESYNC_SECONDS` |
| 数据库表 / 列 | snake_case，单数表名 | `message_locations`、`sent_at` |
| 内部 JSON | snake_case | 账户 `settings` 里的 `host`、`port` |
| 协议载荷 | 跟随规范（JMAP 为 camelCase） | `methodCalls`、`state` |
| 错误枚举 | `<Crate>Error` | `StoreError`、`ProtoError` |

不要用缩写，除非是领域内公认的（IMAP、MIME、UID、TLS、JMAP）。

---

## 3. 错误处理

每个库 crate 定义自己的错误类型，并提供带默认错误参数的别名：

```rust
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;
```

规则：

- 库 crate **不用** `anyhow`；`anyhow` 只用于二进制、示例、测试。
- 跨层错误用 `#[from]` 组合，不把底层字符串再包一层。
- 错误信息写给排障的人：说明哪个操作、哪个对象，不泄露凭据。
- 非测试代码禁止 `unwrap()`/`expect()`；确属不变量时写清原因（如 `expect("session taken")`）。
- 可恢复错误（限流、断线）必须分类，由同步层决定退避；不可恢复错误（认证失败）直接上报。

---

## 4. 异步与并发

- 运行时固定 `tokio`（multi-thread），不在库内部 `block_on`。
- 库层函数是 `async fn`，不隐藏 `spawn`；后台任务只在 `mail-core` / `mail-hub` 出现，并有明确生命周期。
- 共享状态用 `Arc<T>`；需要可变访问的 SQLite 连接用 `Mutex<Connection>`，锁范围只包住一次操作。
- 网络调用必须可取消（`tokio::time::timeout` 或调用方 `select!`）。
- 长连接（IMAP IDLE）每 29 分钟主动重连一次，避免服务端超时。

---

## 5. 时间与 ID

- 持久化时间一律 Unix 秒（`INTEGER`），UTC；展示时再转本地时区。
- 日历另存 IANA 时区 ID 与原始 `.ics`；重复规则不展开存储。
- 内部主键用 `INTEGER PRIMARY KEY`；跨进程/协议暴露的 ID 用字符串（后续 Hub 层映射）。
- 消息去重优先级：`Message-ID` → `(mailbox_id, uid)`。

---

## 6. 数据层

- schema 变更只追加：新增列用 `ALTER TABLE`，并提升 `user_version`；**不得修改已发布的建表语句**。
- 多行写入必须在一个事务里；`apply_fetch` 是唯一写消息的入口。
- 变更日志（`changes`）只增不改；任何写操作都要记一条。
- 外键开启（`PRAGMA foreign_keys=ON`），级联删除按账户 → 邮箱 → 位置。
- 查询必须显式列名，禁止 `SELECT *`。
- FTS5 使用 `tokenize='trigram'`，因此 <3 字符查询走 LIKE 回退；新增搜索入口必须保留该回退。

---

## 7. 协议层

- 一个服务器连接一个 `ImapSession`；不同邮箱切换用 `SELECT`，不并发复用同一连接。
- 能力探测结果缓存到账户配置，不每次重连都探测。
- 增量策略按 `Tier`（A/B/C）分支，禁止在业务层判断具体能力名。
- 删除检测必须带门槛（数量变化或超时），禁止每次全量 `UID SEARCH`。
- 重试只发生在连接建立与 IDLE 重连；单条命令失败按错误类型处理，不盲目重试。

---

## 8. 安全

- 凭据只存 `SecretStore`；数据库只存引用键。
- 日志与错误信息中禁止出现：密码、令牌、完整邮箱地址、正文、附件名。
- 邮件 HTML 统一走净化策略：默认阻断全部外部资源，仅放行 `cid:` 与光栅 data URI；UI 侧禁用 JS。
- 打开外部链接前必须展示真实目标；IDN 同形字告警由 UI 层统一实现。
- 新增加密相关代码必须带 AAD，禁止裸加密。

---

## 9. 测试

- 命名描述行为：`applying_a_fetch_inserts_updates_and_removes`，不用 `test1`。
- 一个测试只验证一件事；断言具体值，不断言“非空”。
- 内存数据库用于存储测试；网络行为用本地 Dovecot 或录制数据。
- 集成测试默认跳过，需环境变量显式开启；CI 中单独的 job 跑。
- 修 bug 必须留回归测试；无法留时在提交信息说明原因。

---

## 10. 依赖政策

新增依赖前依次确认：

1. 标准库或现有依赖能否完成；能则不新增。
2. 维护状态：最近 12 个月有提交、issue 有人处理。
3. 许可与 AGPL-3.0 兼容（MIT/Apache-2.0/BSD/ISC/MPL-2.0 可以；GPL-3 可组合；专有不行）。
4. 体积与编译时间：核心 crate 避免引入大型传递依赖（例如不要为一个小功能引入 `tokio-full`）。
5. 记录：在提交信息里写明“为什么需要它”。

当前核心依赖：`tokio`、`async-imap`、`rustls`/`tokio-rustls`、`rusqlite`、`serde`、`thiserror`、`keyring`、`chacha20poly1305`。

---

## 11. 注释与文档

- 注释解释“为什么”，不重复代码“做了什么”。
- 公开 API 写 `///`，说明失败条件与副作用。
- 模块顶部一句话说明职责。
- 中文只出现在文档与用户可见文案；代码注释与提交信息用英文。
- 文档中的实测数字必须来自真实运行，并注明环境与命令出处。

---

## 12. 提交

- 一个提交一个目的；格式化、重命名与功能改动分开。
- 信息格式：

```
Add XOAUTH2 authentication to IMAP transport

Outlook disables LOGIN, so password auth cannot work. Tokens are stored
through SecretStore and refreshed on 401.
```

- 不提交：`target/`、`spike/data/`、生成的绑定、数据库文件、任何密钥。
- 合并前跑完 §1 的三条命令。

---

## 13. 反例清单（看到就改）

| 反例 | 正确做法 |
|---|---|
| `unwrap()` 处理网络错误 | 分类后返回错误或退避重试 |
| 在 UI 层判断 IMAP 能力 | 由 `mail-proto` 暴露 `Tier` |
| 每次同步全量 `UID SEARCH` | 数量变化或超时才扫描 |
| 把密码写进 settings JSON | 写 `SecretStore`，只存引用 |
| 修改已发布的建表语句 | 追加迁移并提升版本 |
| 在库 crate 引入 `anyhow` | 定义本 crate 错误类型 |
| 注释写“increment i by 1” | 写清业务原因 |
| 为省事新增一个小依赖 | 先确认标准库/现有依赖不可行 |
