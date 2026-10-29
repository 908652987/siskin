# 交接存档

更新日期：2026-10-29
分支：`main`　许可：AGPL-3.0
用途：任何人（或下一次会话）读完本文件即可继续开发，无需回溯聊天记录。

---

## 1. 一句话状态

邮箱同步内核已完成并通过实测（10,002 封全量 1.0 s、增量 5 ms、IDLE 新邮件 43 ms）；**图形界面、日历、Hub 服务端、OAuth2、数据库加密尚未开始**。

---

## 2. 已确认决策（含用户选择，勿擅自更改）

| # | 决策 | 选择 | 说明 |
|---|---|---|---|
| D1 | 核心语言 | Rust | 四端共用一份同步/解析/加密核心 |
| D2 | 界面 | 全原生：SwiftUI（macOS/iOS）、WinUI 3（Windows）、Compose（Android） | 用户明确选择低内存优先，接受三套 UI |
| D3 | 客户端与 Hub 协议 | JMAP 子集（RFC 8620/8621） | 不自创协议 |
| D4 | 部署形态 | 桌面可单机直连；手机与跨设备走自建 Hub | 手机端只做薄客户端 |
| D5 | 存储 | SQLite + FTS5；目标加密 SQLCipher | SQLCipher 尚未接入 |
| D6 | 邮件接入 | IMAP/SMTP，不接 Gmail API / Graph | 规避 restricted scope 与 CASA 年审 |
| D7 | 日历 | 系统桥（EventKit / CalendarContract）+ 直连 CalDAV/Google/Graph | 微软无 CalDAV |
| D8 | 许可 | AGPL-3.0 | 与 GPL-3 代码可组合 |
| D9 | 组件策略 | 优先平台官方与成熟库，只有同步状态机与 HTML 净化自研 | |
| D10 | 首发平台 | macOS + Windows（同一里程碑，先后实现） | |
| D11 | Windows 内存路线 | 先 WinUI 3（目标 110–130 MB），若不接受再评估 C++/WinRT 或 Slint | 未在 Windows 上验证 |

完整理由与代价见 `docs/ARCHITECTURE.md` §2。

---

## 3. 进度矩阵

| 模块 | 状态 | 证据 |
|---|---|---|
| 技术选型与架构 | 完成 | `docs/ARCHITECTURE.md` |
| 五项开工前验证 | 完成 | `docs/SPIKE-REPORT.md` |
| 凭据保险箱（系统钥匙串） | 完成 | `mail-crypto`，macOS Keychain 实跑 |
| SQLite 存储 + 变更日志 + FTS5 | 完成 | `mail-store`，6 个单测 |
| IMAP 连接与三档能力 | 完成 | `mail-proto`，Dovecot/Outlook/iCloud/Fastmail/Yahoo 能力探测 |
| 增量同步引擎 | 完成 | `mail-sync`，3 个单测 + Dovecot 实跑 |
| 常驻 IDLE 与重连退避 | 完成 | `mail-core::watch`，实跑验证 |
| 开发 CLI | 完成 | `mail-cli` |
| 数据库加密（SQLCipher） | **未开始** | 见 §8 第 1 项 |
| OAuth2 / XOAUTH2 | **未开始** | Outlook 当前无法登录 |
| 正文与附件拉取、HTML 渲染 | 未开始 | 仅存头部 |
| 多邮箱遍历 | 部分 | 协议层可列邮箱，门面只同步 INBOX |
| Hub / JMAP | 未开始 | |
| 日历 | 未开始 | |
| 翻译 | 未开始 | |
| 三端 UI | 未开始 | |
| Windows / 移动端验证 | 未开始 | 本机无 Windows、无 .NET SDK |
| 代码规范与风格固化 | 完成 | `AGENTS.md` + `docs/CONVENTIONS.md` + rustfmt/clippy/CI |

---

## 4. 代码地图

```
crates/
  mail-crypto/src/lib.rs   SecretStore trait、KeyringStore、MemoryStore、
                           MasterKey、seal/open（XChaCha20-Poly1305 + AAD）
  mail-store/src/lib.rs    schema、迁移、账户/邮箱/消息/位置/变更日志、
                           apply_fetch（事务）、search_messages（FTS/LIKE 双路）
  mail-store/tests/        6 个存储测试
  mail-proto/src/lib.rs    Stream（TLS/明文）、ImapSession、CapabilitySet、
                           Tier(A/B/C)、fetch_range/fetch_flags/CHANGEDSINCE、
                           list_uids、append、add_flag、move_message、idle_wait
  mail-sync/src/lib.rs     sync_mailbox：UIDVALIDITY 重建、全量/增量、
                           删除检测、合并去重；3 个单测
  mail-core/src/lib.rs     Core：add_account（密码入钥匙串）、sync、
                           watch（IDLE + 指数退避重连）
  mail-cli/src/main.rs     add-account / accounts / sync / watch / list / stats
spike/
  imap-probe/              能力探测与 1 万封压测
  rrule-probe/             重复规则（对照 dateutil）
  html-probe/              邮件 HTML 净化器
  ffi-probe/               UniFFI 示例（Swift/Kotlin 绑定已生成验证）
  swift/                   AppKit 10 万行基准、WKWebView 安全探针、FFI 基准
  scripts/                 测试数据生成、对比、探测脚本
  dovecot/                 测试服务器镜像与配置
docs/
  ARCHITECTURE.md          设计定稿
  SPIKE-REPORT.md          验证报告与实测数据
  IMPLEMENTATION.md        第一段实现说明与缺口
  CONVENTIONS.md           代码风格与工程约定（人读细则）
  HANDOFF.md               本文件
```

同步流程的时序与三档策略见 `docs/IMPLEMENTATION.md` §2。

---

## 5. 如何恢复任务

```bash
# 1) 环境（细节见 §6）
~/.cargo/bin/cargo test --workspace          # 期望 12 个测试全过
~/.cargo/bin/cargo build --release

# 2) 起测试服务器
docker start mail-spike-dovecot               # 若容器不在：见 README 快速开始

# 3) 冒烟
cargo run --release -p mail-cli -- --db /tmp/smoke.db add-account \
  --email vmail@example.com --host 127.0.0.1 --port 31143 --tls false \
  --username vmail --password testpass
cargo run --release -p mail-cli -- --db /tmp/smoke.db sync --email vmail@example.com
cargo run --release -p mail-cli -- --db /tmp/smoke.db stats --email vmail@example.com

# 4) 选下一项：见 §9
```

---

## 6. 环境与工具链（重要，容易踩）

| 项 | 现状 |
|---|---|
| 系统 | macOS 27.0.1 arm64 |
| Rust | 1.99.1，装在 `~/.cargo/bin`；**未修改 PATH**，需用绝对路径或自行 source `~/.cargo/env` |
| Rust 安装源 | 使用 USTC 镜像（`RUSTUP_DIST_SERVER`），官方源 TLS 在本网络被中断 |
| cargo 源 | `~/.cargo/config.toml` 替换为 rsproxy 稀疏索引。**该文件不在仓库内**，换机需重建 |
| Swift | 6.4（Command Line Tools）。**没有 Xcode**：无 `xcodebuild`、无 iOS 模拟器工程模板；`swiftc` 可编译 AppKit/SwiftUI 命令行程序 |
| .NET | 未安装。Windows 验证与 C# 绑定必须换机 |
| Docker | 29.1.3；镜像 `mail-spike-dovecot`（Debian + Dovecot 2.3.19.1，apt 走 USTC 镜像） |
| 测试数据 | `spike/data/Maildir` 10,000 封合成邮件（gitignored，可重新生成） |
| 钥匙串 | `keyring` crate 必须启用特性 `apple-native / windows-native / sync-secret-service / crypto-rust`，否则写入静默失效、读取为空（已踩过） |
| 网络 | 到 `imap.gmail.com:993` 的 TLS 被中断，Gmail 未实测；其余源码镜像可达 |

---

## 7. 实测数据（摘要）

| 场景 | 结果 |
|---|---|
| 首次全量同步 10,002 封（release） | 1,005 ms，数据库 5.1 MB |
| 空闲增量 | 5 ms |
| IDLE 新邮件 → 落库 | 43 ms |
| 外部删除检测 | 69 ms（门槛化后；此前 3.4 s） |
| UIDVALIDITY 重建 | 全量 10,000 封 / 2.8 s（debug） |
| macOS 10 万行列表 | RSS 97 MB，滚动单步 p95 0.74 ms |
| UniFFI 单页 50 行调用 | 73.5 µs |
| 邮件 HTML 净化 | 9 组攻击样本 0 失败；净化后外部请求 0 |
| RRULE | 22 例 21 通过，失败项为夏令时空洞时间，需归一化 |

完整方法与命令见 `docs/SPIKE-REPORT.md`。

---

## 8. 已知缺口（按优先级）

1. **数据库明文**。凭据在钥匙串，但邮件元数据/正文未加密。切 `rusqlite` 的 `bundled-sqlcipher`，密钥存钥匙串。**验收**：`sqlite3 mail.db` 无密钥打不开；`strings mail.db` 搜不到主题。
2. **无 OAuth2**。Outlook 已禁密码登录（`LOGINDISABLED`，仅 `AUTH=XOAUTH2`），当前连接失败。需 XOAUTH2 + PKCE + 刷新令牌。**验收**：Outlook 账户可登录同步，令牌过期自动刷新，密码不落库。
3. **只同步 INBOX**。**验收**：列出邮箱并按 SPECIAL-USE 映射 Sent/Drafts/Trash/Junk，全部可同步。
4. **只存头部**。**验收**：打开邮件时按 BODYSTRUCTURE 拉取正文并缓存，二次打开离线命中；附件按需下载。
5. **变更日志线性增长**。全量同步每封写一条。**验收**：10 万封场景下日志有界（快照 + 增量），状态可由快照重建。
6. **B/C 档未实跑**。**验收**：Outlook（B 档）与 iCloud（C 档）账户实际同步；iCloud 移动邮件走 COPY+删除。
7. **重连未做故障注入**。**验收**：同步中重启服务器，`watch` 按退避恢复且不产生重复行。
8. **中文 1–2 字搜索走 LIKE 扫描**。10 万封内可接受。**验收**：10 万封下 2 字查询 P95 < 150 ms。
9. **Windows/移动端零验证**。**验收**：WinUI 3 调 Rust（uniffi-bindgen-cs），空闲内存 ≤ 160 MB。

---

## 9. 下一步路线（按顺序，独立可交付）

### 第 2 段：安全与认证

1. **SQLCipher**
   - 改动：`mail-store` 启用 `bundled-sqlcipher-vendored-openssl`，`Store::open` 接收密钥；`mail-core` 从钥匙串取主密钥派生库密钥。
   - 验收：§8-1。
2. **OAuth2 + XOAUTH2**
   - 改动：`mail-crypto` 存 refresh token；`mail-proto` 支持 `AUTHENTICATE XOAUTH2`；新增授权流程（回环重定向 + PKCE），CLI 先提供 `--oauth2` 手动授权。
   - 验收：§8-2。

### 第 3 段：内容能力

3. 正文与附件按需拉取；接入 `spike/html-probe` 的净化规则。
4. 多邮箱遍历与角色映射。
5. 变更日志压缩与快照。

### 第 4 段：服务端与移动

6. Hub（axum + JMAP 子集 + 设备配对 + APNs/FCM）。
7. iOS/Android 薄客户端。

### 第 5 段：日历与翻译

8. EventKit / CalendarContract 桥 + CalDAV/Google/Graph；邀请函。
9. 翻译管线（本地/自托管/云）。

### 第 6 段：桌面 UI

10. macOS SwiftUI（列表/阅读/撰写/搜索）。
11. Windows WinUI 3（复用 Rust 核心）。

---

## 10. 待外部输入（没有则无法推进）

| 项 | 用途 | 影响 |
|---|---|---|
| 一个 Gmail 测试账户（应用专用密码） | 验证 Gmail 能力与 Gmail 标签映射 | 未知则 Gmail 路径只能靠推断 |
| 一个 Outlook / M365 测试账户（或 OAuth 客户端） | B 档与 XOAUTH2 验证 | 阻塞 §8-2、§8-6 |
| 一台 Windows 11 机器 + .NET SDK | WinUI 3 + UniFFI 内存验证 | 阻塞 Windows UI 段 |
| APNs / FCM 凭据 | 手机推送 | 阻塞移动端 |
| 是否先做 Hub 还是先做桌面 UI | 排序 | 影响第 4–6 段顺序 |

---

## 11. 风险与陷阱（已踩过，勿重犯）

- `keyring` 缺平台特性会静默不存，必须检查实际钥匙串。
- FTS5 `contentless_delete=1` 必须同时写 `content=''`，否则建表失败。
- trigram 分词器不支持 <3 字符查询，中文需 LIKE 回退。
- iCloud 无 IDLE/CONDSTORE/MOVE；每次全量 UID 扫描代价高（10 万封时不可接受），必须保留“数量变化/超时”门槛。
- `UIDVALIDITY` 变化必须整箱重建，不能复用 UID。
- 草稿与已发送的去重尚未实现，接入发送功能时先解决。
- Mailspring-Sync 为 GPL-3，可与 AGPL 组合，但不要无意引入其他不兼容许可。

---

## 12. 提交与分支约定

风格与规范已固化，不要各写一套：

| 层 | 文件 | 作用 |
|---|---|---|
| 代理入口 | `AGENTS.md` | 每次会话自动加载：必读顺序、开发思路、目录边界、红线 |
| 子代理执行规则 | `.omp/RULES.md` | 8 条策略 + 3 条执行要求；sticky，子代理必收；与 `AGENTS.md` 是子集关系，改动需同步 |
| 人读细则 | `docs/CONVENTIONS.md` | 命名、错误处理、异步、数据层、测试、依赖政策、反例 |
| 工具强制 | `rustfmt.toml`、`[workspace.lints]`、`.editorconfig` | 格式与 lint 基线 |
| CI | `.github/workflows/ci.yml` | fmt 检查 + clippy `-D warnings` + 全量测试 |

- **协作节奏（2026-10-29 用户新增）**：未获确认不开始实现、不改代码；不自动 commit/push；只有大功能完成且用户明确同意才提交。
- 分支：`main`；提交信息用英文祈使句，说明“做了什么 + 为什么”。
- 生成物不入库：`target/`、`spike/data/`、`spike/ffi-probe/gen*`、`*.db*`、spike 编译出的可执行文件。
- 提交前跑：`cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`。
- 分工原则：`AGENTS.md` 全量规范（主会话与其他工具），`.omp/RULES.md` 是子代理可见的执行子集（策略 + 执行要求），`docs/CONVENTIONS.md` 是细则与任务派发模板。**硬约束只在 RULES 与 AGENTS 中各出现一次**。
- OMP 的 task 子代理**不会**收到 `AGENTS.md`（按文件名过滤），只会收到 `.omp/RULES.md`。改动 RULES 时必须同步 AGENTS。
- 已验证（2026-10-29，OMP 18.13.2，新会话 + 实际子代理）：子代理收到 `RULES.md` 内容（sticky），未收到 `AGENTS.md`。
- 不设 `contextFiles.extra`：执行者需要的流程已并入 `RULES.md`，避免两份文件内容漂移。
