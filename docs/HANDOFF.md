# 交接存档

更新日期：2026-11-06
分支：`main`　许可：AGPL-3.0
用途：任何人（或下一次会话）读完本文件即可继续开发，无需回溯聊天记录。

---

## 1. 一句话状态

邮箱同步内核已完成并通过实测（10,002 封全量 1.0 s、增量 5 ms、IDLE 新邮件 43 ms），本地库已切 SQLCipher 加密；接收支持 IMAP 与 POP3，发送支持 SMTP，三者均支持 TLS/STARTTLS（明文需显式开关）；正文与附件已按需拉取（IMAP 走 BODYSTRUCTURE + `BODY.PEEK`，POP3 整封解析），HTML 经 ammonia 净化后入库，二次打开纯离线命中，**正文前 4,000 字已进全文索引**；**`watch` 已覆盖账户下全部可选邮箱**（IDLE 会话池 + 轮询扫尾，只推送真实变更）；IMAP 账户可遍历全部可选邮箱并按 SPECIAL-USE 映射角色，服务器删掉的邮箱进入本地回收站（默认保留 90 天，可配、自动清理）；提供 Gmail/QQ/88/自定义预设；认证为密码/应用专用密码：**Outlook 不接**（微软已禁用密码登录且需 OAuth2 应用身份），**Gmail 走应用专用密码**（待真实账户验证）；变更日志已做快照压缩（默认超过 20,000 行触发，水位不倒退）；**Hub JMAP 子集已到读写**（`mail-api` + `mail-hub`：Session、`Mailbox/get|changes`、`Email/query|get|changes|set`（含 `create`/`destroy`）、`Email/import`、`Thread/get|changes`、多副本移动、`/upload`、SSE、附件下载、设备吊销、配对与 Bearer 鉴权，正文按需拉取；APNs/FCM 未做；**Hub 已内置后台同步**（IDLE 池 + 扫尾，进程内按账户启动/重连）、`/healthz`、容器镜像与卷内文件密钥后端/主密钥注入）；**macOS 客户端已可收发**：列表/阅读/搜索、撰写（To/Cc/Bcc + 多附件，`mail-ffi::send`）与 `dist/Siskin.app` 打包完成，通知未做；Linux 桌面 UI 技术栈定为 Slint（D12），Windows UI、日历、手机客户端尚未开始。

---

## 2. 已确认决策（含用户选择，勿擅自更改）

| # | 决策 | 选择 | 说明 |
|---|---|---|---|
| D1 | 核心语言 | Rust | 四端共用一份同步/解析/加密核心 |
| D2 | 界面 | 全原生：SwiftUI（macOS/iOS）、WinUI 3（Windows）、Compose（Android） | 用户明确选择低内存优先，接受三套 UI |
| D3 | 客户端与 Hub 协议 | JMAP 子集（RFC 8620/8621） | 不自创协议 |
| D4 | 部署形态 | 桌面可单机直连；手机与跨设备走自建 Hub | 手机端只做薄客户端 |
| D5 | 存储 | SQLite + FTS5；目标加密 SQLCipher | SQLCipher 已接入，密钥由钥匙串主密钥 HKDF 派生 |
| D6 | 邮件接入 | IMAP/POP3 接收 + SMTP 发送，不接 Gmail API / Graph；Outlook 不做（仅 OAuth2），Gmail 走应用专用密码 | 规避 restricted scope 与 CASA 年审；OAuth2 需自建 Entra 应用身份，测试账号无租户无法验收 |
| D7 | 日历 | 系统桥（EventKit / CalendarContract）+ 直连 CalDAV/Google/Graph | 微软无 CalDAV |
| D8 | 许可 | AGPL-3.0 | 与 GPL-3 代码可组合 |
| D9 | 组件策略 | 优先平台官方与成熟库，只有同步状态机与 HTML 净化自研 | |
| D10 | 首发平台 | macOS + Windows（同一里程碑，先后实现） | |
| D11 | Windows 内存路线 | 先 WinUI 3（目标 110–130 MB），若不接受再评估 C++/WinRT 或 Slint | 未在 Windows 上验证 |
| D12 | Linux 桌面 UI 技术栈 | Slint（纯 Rust，单进程直连 `mail-core`） | 无 WebView / GTK / Qt 运行时依赖；macOS 上即可编译运行验证。代价：第四套 UI，Windows 若也转 Slint 需重评 D11 |
| D13 | 客户端双模式形态 | 首次运行选择本地模式或远程模式；远程模式只填服务端地址 + 设备令牌，邮箱凭据只在服务端；客户端缓存列表元数据与已打开正文/附件，可一键清除；服务端地址必须 HTTPS（回环例外），令牌按账户授权 | 把 D4 落成可操作产品形态；远程模式客户端不接触邮箱密码；列表可全量缓存，正文/附件按需；服务端提交端点与令牌按账户授权已实现，客户端适配层待做 |

完整理由与代价见 `docs/ARCHITECTURE.md` §2。

---

## 3. 进度矩阵

| 模块 | 状态 | 证据 |
|---|---|---|
| 技术选型与架构 | 完成 | `docs/ARCHITECTURE.md` |
| 五项开工前验证 | 完成 | `docs/SPIKE-REPORT.md` |
| 凭据保险箱（系统钥匙串） | 完成 | `mail-crypto`，macOS Keychain 实跑 |
| SQLite 存储 + 变更日志 + FTS5 | 完成 | `mail-store`，9 个单测 |
| 数据库加密（SQLCipher） | 完成 | `mail-store` + `mail-core`；无密钥/错密钥打不开，10,003 封 612 ms；主密钥可 `export-key`/`import-key` 人工备份与恢复 |
| IMAP 连接与三档能力 | 完成 | `mail-proto`，Dovecot/Outlook/iCloud/Fastmail/Yahoo 能力探测 |
| 增量同步引擎 | 完成 | `mail-sync`，3 个单测 + Dovecot 实跑 |
| 常驻 IDLE 与重连退避 | 完成 | `mail-core::watch_account`；IDLE 会话池（默认 4）+ 扫尾连接（默认 120 s），Dovecot 6 邮箱实跑：非 INBOX 新邮件秒级入库、重启后重连、消失邮箱停 worker |
| 开发 CLI | 完成 | `mail-cli`（含 `search`） |
| IMAP/POP3/SMTP + STARTTLS | 完成 | 三类协议对 88 服务器 143/110/587 实机升级到 AUTH；密码/STLS/STARTTLS 路径 |
| POP3 接收同步 | 完成（mock） | `mail-proto::pop3` + `mail-sync::pop3`；UIDL 去重、轮询、单 INBOX；真实账户未验证 |
| SMTP 发信 | 完成（协议层） | `mail-proto::smtp` + `core.send`；纯文本、base64 正文；真实投递未验证 |
| 提供商预设 | 完成 | `gmail/qq/88/custom`；QQ/88/Gmail 主机与端口已用登录尝试验证 |
| Outlook / Hotmail | **不做（范围决策）** | 微软已禁用密码登录，仅 OAuth2；测试账号无 Entra 租户无法取得应用身份 |
| Gmail（应用专用密码） | 待真实验证 | 密码路径已就绪；需 Gmail 开启 IMAP + 2FA 应用专用密码 |
| 正文与附件拉取 | 完成 | `mail-proto` BODYSTRUCTURE + `BODY.PEEK`；`mail-parse` 解码与净化；Dovecot 实跑，二次打开离线 39 ms |
| 正文全文索引 | 完成 | `mail-store` schema v6：`messages.body_text` + FTS `body_text` 列；前 4,000 字入库，旧库迁移从 blob 回填、打开邮件时自愈；Dovecot 实跑正文与中文 2 字可搜 |
| 多邮箱遍历与角色映射 | 完成 | `mail-sync::sync_account`；SPECIAL-USE 优先、名字/中文回退；修改版 UTF-7 名称；Dovecot 6 邮箱实跑 |
| 本地回收站与保留期 | 完成 | `mail-store::missing_at`/`purge_trash`/`settings`；删除邮箱进回收站，默认 90 天，`sync` 自动清理；同名单重建保留旧数据 |
| 变更日志压缩与快照 | 完成 | `mail-store::compact_changes` + `change_snapshots`；默认 20,000 行阈值，`sync`/`sync_account`/`watch` 每轮同步后自动执行；Dovecot 10,010 条压缩后日志归零、水位不倒退 |
| HTML 渲染 | 未开始 | 净化器已就绪（`mail-parse::sanitize_html`），UI WebView 段未做 |
| JMAP 子集（读写） | 完成 | `mail-api`：Session、`Mailbox/get|changes`、`Email/query|get|changes|set`（create/destroy）、`Email/import`、`Thread/get|changes`、`Identity/get`、`EmailSubmission/get|set`、`BodySource` 按需正文、`WriteSink`/`SubmissionSink` 写路径、账户作用域 `session_for`/`handle_for_account`；32 个 API 测试；Dovecot 实跑 |
| Hub 服务端与设备配对 | 完成 | `mail-hub`（axum）：配对码换令牌、库中只存哈希、Bearer 鉴权、`/eventsource`、`/download`；`mail-cli devices`/`revoke-device`（`devices` 显示绑定账户）；10 个路由测试 + 实跑 |
| Hub 远程模式前置（HTTPS / 发送 / 账户授权） | 完成（TLS 仍由反代终止） | 设备令牌已绑定账户（`hub_devices.account_id`，Session/upload/download/SSE/方法级 `accountId` 全部校验）；发送走标准 `Identity/get` + `EmailSubmission/set`（create）；后台同步与 `/healthz` 已内置；**剩余**：TLS 仍由反代终止（Dockerfile 注释与 ARCHITECTURE §3.2 的清单） |
| Hub 后台同步与部署底线 | 完成 | `mail-hub` 进程内按账户启动 `watch_account`（IDLE 池 + 扫尾，默认 4/120 s，`EYRIE_HUB_SYNC` 可关）；`GET /healthz`（匿名）：库可读、账户数、`lastSyncAt`、watchers/events；`Dockerfile`（多阶段、非 root、`HEALTHCHECK`）+ `.dockerignore`；文件密钥后端 `EYRIE_HUB_SECRET_DIR` 与 `EYRIE_HUB_MASTER_KEY_FILE` 注入；Dovecot 实跑：不跑 CLI 时投递 4 s 内 `Email/query` total 10026→10027，master key 空目录+key 文件可开库，缺 key 启动报 `database key does not match`；容器实跑：`/healthz` 200、`docker inspect` healthy、投递后 total 10027→10028 |
| 客户端双模式（本地 / 远程） | 未开始 | macOS 目前只有本地直连（FFI → `mail-core`）；无 JMAP 客户端、无远程配置入口、无独立缓存与清缓存 |
| Hub 写操作与 SSE | 完成 | `Email/set` 已读/标志/移动（含多副本）/创建/永久删除、`Mailbox/changes`、SSE 状态事件、blob 下载；Dovecot 实跑（`\Seen` 置/清、移动、计数、下载、token 吊销后 401） |
| Hub 发送（EmailSubmission） | 完成 | `Identity/get`（每账户一个身份）、`EmailSubmission/get|set`（create、`onSuccessUpdateEmail` 隐式 `Email/set`、`sendAt` 拒绝、`undoStatus=final`）；原始报文随 `Email/import|create` 入库（`messages.raw_blob_hash`）；服务端推导 envelope、投递前剥除 `Bcc` 头；Dovecot + 本地 SMTP sink 实跑：3 个 RCPT（含 Bcc）、服务端副本无 Bcc、Sent +1 |
| Hub 账户隔离 | 完成 | 配对必须给账户（多账户时），`hub_devices.account_id`；Session 只列绑定账户；`/upload`、`/download`、SSE 与每个 JMAP 方法校验 `accountId`；跨账户返回 404/`accountNotFound`；单账户旧库自动回填，多账户保持未绑定并要求重新配对；10 个路由测试 + 实跑 |
| Hub 上传与导入 | 完成 | `/upload`（内容寻址 + 24 h 回收）+ `Email/import`（APPEND → 同步 → 返回本地 id）；Dovecot 实跑 2 封导入、首封带 `\Seen` |
| 线程（Thread/get\|changes） | 完成 | schema v8 `in_reply_to`；`thread_id` 取父线程根，父后到递归改挂；`Email/get.threadId` 与 `Thread/get` 一致；线程变更由写侧记录（created/updated/removed）；单元 9 项 + Dovecot 实跑「回复并入同一线程」「根 created、回复 updated、销毁后 destroyed」 |
| Hub APNs/FCM 推送 | 未开始 | 依赖推送凭据；SSE 已可替代桌面/局域网客户端 |
| 日历 | 未开始 | |
| 翻译 | 未开始 | |
| UniFFI 门面 | 完成 | `mail-ffi`：账户/邮箱/列表/正文/关键字/移动/同步；Swift 绑定生成、`SiskinSmoke` 实跑 |
| macOS UI（列表/阅读/搜索） | 完成（主路径） | `apps/macos` SwiftUI：账户选择/新增、邮箱/邮件列表、HTML+纯文本阅读、搜索、同步、标已读/未读、标星、移动、分页加载、附件保存；SwiftPM 构建 + 10,016 封库实跑 |
| macOS 国际化 | 完成 | zh-Hans / en `Localizable.strings`；系统语言自动选择，`SISKIN_LANG` 可强制；中英文快照各一张 |
| 账户预设 | 完成 | Gmail / QQ / 88 / 自定义快捷按钮；IMAP/SMTP 主机、端口、TLS 由 `mail-ffi::provider_presets` 提供，88 按邮箱域名推导 |
| macOS 撰写 | 完成 | `ComposeView` + `mail-ffi::send`：To/Cc/Bcc、主题、纯文本正文、多附件（NSOpenPanel + UTType MIME）、发送失败保留草稿；⌘N / 工具栏入口；Dovecot + 本地 SMTP sink 实发，`Sent` 归档 |
| macOS `.app` 打包 | 完成 | `apps/macos/package.sh`：release 静态链接 + `Info.plist`（`com.eyrie.siskin`）+ 资源 bundle + ad-hoc 签名；`dist/Siskin.app` 直接启动并渲染撰写快照 |
| macOS 通知 | 未开始 | 需要 FFI 事件流与通知权限，单独一段 |
| Windows / Linux / 移动 UI | 未开始 | Linux 栈已定 Slint（D12）；Windows/iOS/Android 仍按 D2 |
| Windows / 移动端验证 | 未开始 | 本机无 Windows、无 .NET SDK |
| 代码规范与风格固化 | 完成 | `AGENTS.md` + `docs/CONVENTIONS.md` + rustfmt/clippy/CI |

---

## 4. 代码地图

```
Dockerfile                 Hub 镜像（多阶段；非 root UID 10001；HEALTHCHECK /healthz）
.dockerignore              构建上下文排除 target/apps/generated
crates/
  mail-crypto/src/lib.rs   SecretStore trait、KeyringStore、FileStore（dev/服务端文件后端）、MemoryStore、
                           MasterKey（HKDF 发送子密钥）、seal/open（XChaCha20-Poly1305 + AAD）
  mail-store/src/lib.rs    SQLCipher 打开（PRAGMA key + 错密钥/明文库拒绝）、schema、
                           迁移（v6 重建 FTS 并从 blob 回填正文；v8 加 in_reply_to
                           与 uploads；v9 加 hub_devices.account_id、messages.raw_blob_hash
                           与 email_submissions）、账户/邮箱/消息/位置、
                           变更日志（message/mailbox/thread 三类）、settings、
                           apply_fetch（事务，线程变更随消息写入）、
                           search_messages（FTS/LIKE 双路，含正文）、
                           rewrite_fts / index_body_text / index_body_text_if_missing、
                           list/search/count 的多邮箱去重与回收站过滤、
                           missing_at / mark_missing_mailboxes / retire_mailbox、
                           list_trash / purge_trash（回收站保留期）、
                           blobs（内容寻址 + 引用计数）、message_parts/attachments、
                           record_body_structure/store_body_parts/store_attachment、
                           attach_raw_blob/raw_blob（客户端上传的原始报文）、
                           create_submission/submissions（Hub 提交记录）、
                           threads/list_threads/thread_for_message/adopt_descendants、
                           store_upload/upload_for_account/purge_expired_uploads、
                           message_by_message_id/message_at/mailbox_by_role、
                           remove_location/remove_all_locations、
                           changes_since/latest_change_seq/latest_change_seq_type/change_window、
                           account_state/latest_snapshot/compact_changes、
                           change_log_max_rows
  mail-store/tests/        49 个存储测试（含错密钥、明文库、落盘无明文、blob 回收、
                           回收站与保留期、变更日志快照与压缩、正文索引与截断、
                           v5→v6、v7→v8 与 v8→v9 迁移、标志位/位置移动、blob 账户隔离、
                           设备绑定与吊销、原始报文生命周期、提交记录、
                           线程归并与乱序父到达、线程变更日志、上传隔离与过期回收）
  mail-store/src/lib.rs    migration_tests：v5 库（四列 FTS、无 body_text）升级后
                           主题索引保留、正文从 blob 回填；v9 单账户回填设备绑定、
                           多账户保持未绑定
  mail-proto/src/lib.rs    Stream（TLS/明文/STARTTLS）、ImapSession、CapabilitySet、
                           Tier(A/B/C)、fetch_range/fetch_flags/CHANGEDSINCE、
                           list_uids、fetch_body_layout（BODYSTRUCTURE 扁平化）、
                           fetch_part（BODY.PEEK）、append、add_flag、move_message、
                           delete_message（STORE \Deleted + UID EXPUNGE）、idle_wait、
                           list_mailboxes + SPECIAL-USE、修改版 UTF-7 名称
  mail-proto/src/utf7.rs   RFC 3501 修改版 UTF-7 编解码
  mail-parse/src/lib.rs    MIME 解码（base64/QP/charset/RFC 2047）、ammonia 净化、
                           HTML→纯文本、整封解析（POP3）
  mail-proto/src/pop3.rs   POP3：LIST/UIDL/TOP/RETR、STLS、USER/PASS
  mail-proto/src/smtp.rs   SMTP：EHLO、STARTTLS、AUTH PLAIN/LOGIN、DATA
  mail-sync/src/lib.rs     sync_mailbox：UIDVALIDITY 重建、全量/增量、
                           删除检测、合并去重；sync_account / sync_account_except
                           （跳过已有 IDLE 会话的邮箱）：全邮箱遍历、
                           \Noselect/\All/\Flagged 过滤、角色映射、消失邮箱转回收站
  mail-sync/src/pop3.rs    POP3 镜像：UIDL 去重、头部解析、轮询、删除跟随服务器
  mail-core/src/lib.rs     Core：主密钥（钥匙串）→ HKDF → 库密钥、
                           add_account（密码入钥匙串）、协议分派、sync/sync_account、
                           watch（单邮箱）/ watch_account（全邮箱：IDLE 池 + 扫尾，
                           只推送真实变更）、check、send（SMTP + Sent 归档）、
                           send_raw（客户端提供的 RFC 822 + envelope）、
                           fetch_body（缓存优先，命中时补写正文索引）、fetch_attachment（按需）、
                           set_keywords（JMAP 关键字 → IMAP STORE）、
                           move_message（MOVE 或 COPY+EXPUNGE；多副本保留一份、其余进 Trash）、
                           destroy_message（每个副本永久删除）、
                           purge_expired_trash / trash_retention_days
  mail-core/src/compose.rs OutgoingMessage → RFC 5322：To/Cc/Bcc（Bcc 只进信封）、
                           头注入校验、2047 主题/附件名、multipart/mixed、25 MiB 上限；
                           derive_envelope（从 To/Cc/Bcc 头推导信封）、strip_bcc_header
  mail-core/src/presets.rs gmail/qq/88/custom 主机与默认端口
  mail-api/src/lib.rs      JMAP 子集：Session（含 submission 能力与 session_for）、
                           Mailbox/get|changes、
                           Email/query|get|changes|set|import（create/destroy）、
                           Thread/get|changes、Identity/get、EmailSubmission/get|set、
                           BodySource 按需正文、WriteSink/SubmissionSink 写路径
                           （含 import/destroy）、handle_for_account 账户隔离、
                           mailboxIds/<id> 子补丁、filter/sort 的显式拒绝
  mail-api/tests/          32 个 API 测试（邮箱计数、筛选分页、正文、变更语义、
                           线程变更、关键字写入、移动、state 冲突、blobId、导入、
                           创建/销毁、错误、身份、提交与 envelope、
                           onSuccessUpdateEmail、账户隔离）
  mail-hub/src/main.rs     axum 服务：/.well-known/jmap、/jmap、/pair、/eventsource、
                           /download、/upload（两种斜杠形式）；CoreBodies/CoreWrites/
                           CoreSubmission 同文件 tests：10 个路由测试（配对与账户绑定、
                           鉴权、JMAP、跨账户拒绝、下载、上传、SSE）
  mail-ffi/src/lib.rs      UniFFI 门面：provider_presets + MailClient（open/accounts/add_account/mailboxes/
                           messages/body/set_seen/move_message/sync/send）、FfiError
  mail-ffi/src/bin/        uniffi-bindgen
  mail-cli/src/main.rs     add-account / check / send（--cc/--bcc/--attach）/ accounts / sync / watch（省略
                           --mailbox 即全邮箱，--idle-sessions/--sweep-seconds 可调）/
                           mailboxes / list / search / trash / trash-purge / body /
                           attachment / keywords / move / stats / retention /
                           set-retention / changes / changes-compact /
                           set-change-max-rows / devices / revoke-device /
                           export-key / import-key
apps/
  macos/Package.swift     SwiftPM：SiskinApp（SwiftUI）+ SiskinSmoke + MailFFI + mail_ffiFFI
  macos/Sources/SiskinApp main.swift（AppKit 引导、菜单）、AppModel、Views、ComposeView、L10n、MessageWebView
  macos/Sources/SiskinSmoke 无界面冒烟：账户/邮箱/列表/正文/关键字往返 + 可选 SISKIN_SEND_TO 发送
  macos/build.sh          编译 mail-ffi → 生成 Swift 绑定 → swift build（debug，开发用）
  macos/package.sh        同上 + release 构建 → Siskin.app（Info.plist、资源 bundle、ad-hoc 签名）
  macos/Info.plist        bundle 标识 com.eyrie.siskin、最低 macOS 14
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
~/.cargo/bin/cargo test --workspace          # 期望 166 个测试全过
~/.cargo/bin/cargo build --release
export EYRIE_DEV_SECRET_DIR=/tmp/eyrie-dev-secrets   # 开发期绕开钥匙串；发布验证不要设置

# 2) 起测试服务器
docker start mail-spike-dovecot               # 若容器不在：见 README 快速开始

# 3) 冒烟
cargo run --release -p mail-cli -- --db /tmp/smoke.db add-account \
  --email vmail@example.com --host 127.0.0.1 --port 31143 \
  --security plaintext --insecure-plaintext true \
  --username vmail --password testpass
cargo run --release -p mail-cli -- --db /tmp/smoke.db sync --email vmail@example.com   # 全部可选邮箱
cargo run --release -p mail-cli -- --db /tmp/smoke.db mailboxes --email vmail@example.com
cargo run --release -p mail-cli -- --db /tmp/smoke.db stats --email vmail@example.com
cargo run --release -p mail-cli -- --db /tmp/smoke.db check --email vmail@example.com

# 3.1) 回收站：服务器删掉的邮箱进回收站，默认保留 90 天
docker exec mail-spike-dovecot doveadm mailbox create -u vmail 项目
docker exec mail-spike-dovecot doveadm mailbox delete -u vmail 项目   # 下次 sync 后变 missing
cargo run --release -p mail-cli -- --db /tmp/smoke.db trash --email vmail@example.com
cargo run --release -p mail-cli -- --db /tmp/smoke.db retention
cargo run --release -p mail-cli -- --db /tmp/smoke.db set-retention --days 30
cargo run --release -p mail-cli -- --db /tmp/smoke.db trash-purge

# 3.2) 正文与附件：id 取自 list 的 id= 列，附件 part 取自 body 的 stderr 输出
cargo run --release -p mail-cli -- --db /tmp/smoke.db list --email vmail@example.com --limit 5
cargo run --release -p mail-cli -- --db /tmp/smoke.db body --email vmail@example.com --id 42
cargo run --release -p mail-cli -- --db /tmp/smoke.db attachment --email vmail@example.com \
  --id 42 --part 2 --out /tmp/attachment.bin
# 正文在 body 拉取时进索引：拉取前搜不到，拉取后 search 命中；中文 1–2 字走 LIKE
cargo run --release -p mail-cli -- --db /tmp/smoke.db search --email vmail@example.com --query 主题词
cargo run --release -p mail-cli -- --db /tmp/smoke.db search --email vmail@example.com --query 营收

# 3.3) 常驻监听：省略 --mailbox 即覆盖全部可选邮箱（IDLE 池 4 + 扫尾 120 s）
cargo run --release -p mail-cli -- --db /tmp/smoke.db watch --email vmail@example.com
cargo run --release -p mail-cli -- --db /tmp/smoke.db watch --email vmail@example.com \
  --idle-sessions 0 --sweep-seconds 2            # 纯轮询路径，用于验证扫尾
cargo run --release -p mail-cli -- --db /tmp/smoke.db watch --email vmail@example.com --mailbox INBOX
# 事件只在有新增/删除/重建时触发；--max-events N 在收到 N 个这样的事件后退出
# 投递测试邮件必须带 -i，否则 doveadm 读到 EOF 存进空邮件：
#   printf 'From: a@example.com\r\nSubject: probe\r\n\r\nbody\r\n' | \
#     docker exec -i mail-spike-dovecot doveadm save -u vmail -m INBOX

# 3.4) 变更日志：默认超过 20,000 行在 sync 结束时自动压缩；也可手动触发
cargo run --release -p mail-cli -- --db /tmp/smoke.db changes --email vmail@example.com
cargo run --release -p mail-cli -- --db /tmp/smoke.db set-change-max-rows --rows 1000
cargo run --release -p mail-cli -- --db /tmp/smoke.db changes-compact --email vmail@example.com
# 压缩后 changes=0、snapshot_seq=latest_seq，`list/stats` 数据不受影响

# 4) 主密钥备份 / 恢复（输出即密钥本身：存进密码管理器，勿贴日志或提交）
cargo run --release -p mail-cli -- --db /tmp/smoke.db export-key
cargo run --release -p mail-cli -- --db /tmp/smoke.db import-key --value '<base64>'
# 换机器时先 import-key 再打开库；库已存在则密钥必须能打开它，否则拒绝写入

# 4.1) Hub：与 mail-cli 共用同一个库与密钥存储
# 示例用 18080 覆盖默认 8080；端口由 EYRIE_HUB_BIND 决定，起服务前用 lsof 查占用
# 后台同步默认开启：每账户一个 watch_account（IDLE 池 + 扫尾）；
# EYRIE_HUB_SYNC=0 关闭，EYRIE_HUB_SWEEP_SECONDS / EYRIE_HUB_IDLE_SESSIONS 调整参数
EYRIE_DEV_SECRET_DIR=/tmp/eyrie-dev-secrets EYRIE_HUB_DB=/tmp/smoke.db \
EYRIE_HUB_BIND=127.0.0.1:18080 EYRIE_HUB_PAIRING_CODE=dev-code \
EYRIE_HUB_SWEEP_SECONDS=120 EYRIE_HUB_IDLE_SESSIONS=4 \
cargo run -p mail-hub --release
# 匿名健康检查：库可读、账户数、lastSyncAt、同步任务状态；库打不开返回 503
curl -s http://127.0.0.1:18080/healthz
# {"status":"ok","database":"ok","accounts":1,"lastSyncAt":...,"sync":{"enabled":true,
#  "watchers":1,"events":N,"lastEventAt":...,"sweepSeconds":120,"idleSessions":4}}
TOKEN=$(curl -s -X POST http://127.0.0.1:18080/pair -H 'Content-Type: application/json' \
  -d '{"code":"dev-code","name":"phone","accountId":"1"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')
# 单账户库可省略 accountId；多账户时必须给出，否则 400。令牌只在该账户内有效。
curl -s http://127.0.0.1:18080/.well-known/jmap -H "Authorization: Bearer $TOKEN"
# Session 只列出绑定账户，并带 urn:ietf:params:jmap:submission 能力
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Mailbox/get",{"accountId":"1"},"c0"]]}'
# 写：标已读（id 取 Email/get）、移动（mailboxId 取 Mailbox/get）
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/set",{"accountId":"1","update":{"<id>":{"keywords":{"$seen":true}}}},"c0"]]}'
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/set",{"accountId":"1","update":{"<id>":{"mailboxIds":{"<mailboxId>":true}}}},"c0"]]}'
# 增量与 SSE：closeafter/ping 接受秒数或 PTxxS
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Mailbox/changes",{"accountId":"1","sinceState":"<seq>"},"c0"]]}'
curl -sN "http://127.0.0.1:18080/eventsource/?types=Email,Mailbox&closeafter=5&ping=2" \
  -H "Authorization: Bearer $TOKEN"
# 下载：blobId 取 Email/get 的 textBody/attachments；只在 blob 属于该账户时返回
curl -s "http://127.0.0.1:18080/download/1/<blobId>/file.bin" \
  -H "Authorization: Bearer $TOKEN" -o /tmp/download.bin
# 上传 → 导入 → 线程（blobId 取 /upload 响应，mailboxId 取 Mailbox/get）
TOKEN=...  # /pair 返回
curl -s -X POST http://127.0.0.1:18080/upload/1 -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: message/rfc822' --data-binary @/tmp/imp1.eml
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/import",{"accountId":"1","emails":{"a":{"blobId":"<blob>","mailboxIds":{"1":true},"keywords":{"$seen":true}}}},"c0"]]}'
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Thread/get",{"accountId":"1","ids":["<Message-ID>"]},"c0"]]}'
# Email/set create（返回完整 Email 对象）与 destroy（永久删除所有副本）
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/set",{"accountId":"1","create":{"k1":{"blobId":"<blob>","mailboxIds":{"3":true},"keywords":{"$seen":true}}}},"c0"]]}'
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/set",{"accountId":"1","destroy":["<id>"]},"c0"]]}'
# destroy 后：Email/get 进 notFound，服务器 doveadm search 无结果；多副本会一起删除
# 多副本移动：目标保留一份，其余副本进 \Trash 角色邮箱（无 Trash 时 notSupported）
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/set",{"accountId":"1","update":{"<id>":{"mailboxIds":{"<mailboxId>":true}}}},"c0"]]}'
# Thread/changes：新线程 created、回复并入 threaded updated、最后一份副本删除后 destroyed
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Thread/changes",{"accountId":"1","sinceState":"<state>"},"c0"]]}'
# 尾斜杠同样可用；导入后 doveadm fetch 可核对服务器上的 flags 与 In-Reply-To
# 4.1.1) 远程发送：上传 → Email/import 到 Drafts → EmailSubmission/set
# blobId 取 /upload 响应，emailId 取 Email/import 的 created id，identityId 为 "i<accountId>"
curl -s -X POST http://127.0.0.1:18080/upload/1 -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: message/rfc822' --data-binary @/tmp/msg.eml
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Email/import",{"accountId":"1","emails":{"d1":{"blobId":"<blob>","mailboxIds":{"<drafts>":true},"keywords":{"$draft":true}}}},"c0"]]}'
# envelope 可省略（服务端按 To/Cc/Bcc 头推导）；给 envelope 时 Bcc 只放在这里
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail","urn:ietf:params:jmap:submission"],"methodCalls":[["EmailSubmission/set",{"accountId":"1","create":{"k1":{"emailId":"<id>","identityId":"i1","envelope":{"mailFrom":{"email":"vmail@example.com"},"rcptTo":[{"email":"peer@example.com"}]}}},"onSuccessUpdateEmail":{"#k1":{"keywords/$seen":true}}},"c0"]]}'
# 成功时返回两条 methodResponses：EmailSubmission/set 与隐式 Email/set（同一 call id）；
# 投递前服务端删除报文里的 Bcc 头，Sent 归档由内核对 IMAP 账户执行（失败只体现在 saveError）
curl -s -X POST http://127.0.0.1:18080/jmap -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail","urn:ietf:params:jmap:submission"],"methodCalls":[["EmailSubmission/get",{"accountId":"1","ids":["1"]},"c0"]]}'
# 只接受客户端上传/导入过的消息（有原始报文）；同步来的邮件提交返回 invalidEmail
# 4.1.2) 容器部署（仓库根 Dockerfile；TLS 仍由前置反代终止）
docker build -t siskin-hub .
# 库放 /data 卷；密钥目录（主密钥 + 账户密码）单独挂卷，UID 10001 需可写
docker run --rm -p 127.0.0.1:18080:8080 \
  -v /srv/siskin/data:/data -v /srv/siskin/secrets:/secrets \
  -e EYRIE_HUB_SECRET_DIR=/secrets -e EYRIE_HUB_PAIRING_CODE=dev-code \
  siskin-hub
# 主密钥也可由 Docker secret 注入（key 文件与库不同卷）；缺 key/错 key 启动即失败：
#   EYRIE_HUB_MASTER_KEY_FILE=/run/secrets/master-key
# 镜像内 HEALTHCHECK 轮询 /healthz；绑定 0.0.0.0 只在容器网络内，对外只暴露反代端口
# 设备管理（与 Hub 共用库）
cargo run -p mail-cli -- --db /tmp/smoke.db devices
cargo run -p mail-cli -- --db /tmp/smoke.db revoke-device --id <deviceId>
# CLI 直连同一条写路径
cargo run -p mail-cli -- --db /tmp/smoke.db keywords --email vmail@example.com --id <id> --add '$seen'
cargo run -p mail-cli -- --db /tmp/smoke.db move --email vmail@example.com --id <id> --mailbox 归档
# 未设置 EYRIE_HUB_PAIRING_CODE 时，配对码每进程随机生成并打印一次；令牌只在配对响应里返回

# 4.2) macOS SwiftUI 壳（与 CLI 共用库与密钥存储）
./apps/macos/build.sh
SISKIN_DB=/tmp/smoke.db EYRIE_DEV_SECRET_DIR=/tmp/eyrie-dev-secrets \
  apps/macos/.build/debug/Siskin
# 无界面冒烟：账户/邮箱/列表/正文/关键字往返
SISKIN_DB=/tmp/smoke.db EYRIE_DEV_SECRET_DIR=/tmp/eyrie-dev-secrets \
  apps/macos/.build/debug/SiskinSmoke
# 应用内渲染窗口快照（无需屏幕录制权限）；SISKIN_SNAPSHOT_MESSAGE 可预选邮件
SISKIN_DB=/tmp/smoke.db EYRIE_DEV_SECRET_DIR=/tmp/eyrie-dev-secrets \
  SISKIN_SNAPSHOT=/tmp/ui.png SISKIN_SNAPSHOT_MESSAGE=711 \
  apps/macos/.build/debug/Siskin

# 4.3) 撰写：CLI 走同一条 compose/SMTP 路径；本地 SMTP sink 见下
# sink 需支持 AUTH PLAIN（Python 3.9 自带 smtpd 不支持，写 20 行的 asyncio 版本即可）
cargo run -p mail-cli -- --db /tmp/smoke.db send --email vmail@example.com \
  --to peer@example.com --cc copy@example.com --bcc secret@example.com \
  --subject 撰写测试 --body 'hello' --attach /tmp/note.txt
# 断言：sink 收到 3 个 RCPT、报文头无 Bcc；sync 后 Sent 计数 +1
# FFI 冒烟发送（SiskinSmoke）
SISKIN_DB=/tmp/smoke.db EYRIE_DEV_SECRET_DIR=/tmp/eyrie-dev-secrets \
  SISKIN_SEND_TO=peer@example.com SISKIN_SEND_CC=copy@example.com \
  SISKIN_SEND_BCC=secret@example.com SISKIN_SEND_ATTACH=/tmp/note.txt \
  apps/macos/.build/debug/SiskinSmoke
# 撰写 sheet 快照：SISKIN_SNAPSHOT_COMPOSE=1 + SISKIN_COMPOSE_TO/CC/BCC/SUBJECT/BODY/ATTACH 预填
# 改了 FFI 公开类型后先重跑 build.sh（package.sh 也会重新生成绑定）
# 4.4) .app 打包（release 静态链接 + Info.plist + ad-hoc 签名）
./apps/macos/package.sh
SISKIN_DB=/tmp/smoke.db EYRIE_DEV_SECRET_DIR=/tmp/eyrie-dev-secrets \
  apps/macos/dist/Siskin.app/Contents/MacOS/Siskin
# Finder 启动不带环境变量：会走钥匙串并按 cdhash 再弹一次授权，属预期

# 5) 选下一项：见 §9
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
| Docker | 29.1.3；镜像 `mail-spike-dovecot`（Debian + Dovecot 2.3.19.1，apt 走 USTC 镜像）。Hub 镜像在仓库根 `Dockerfile`：`docker build --network=host -t siskin-hub .`（本机网络下 buildkit 默认网络访问 Debian 源会 502，host 网络正常；Dockerfile 内已把 Debian 源换成 USTC）。构建需 `libdbus-1-dev`/`pkg-config`（keyring 依赖链接 libdbus），运行镜像装 `libdbus-1-3` |
| 测试数据 | `spike/data/Maildir` 10,000 封合成邮件（gitignored，可重新生成） |
| 钥匙串 | `keyring` crate 必须启用特性 `apple-native / windows-native / sync-secret-service / crypto-rust`，否则写入静默失效、读取为空（已踩过） |
| SQLCipher | `rusqlite` 特性 `bundled-sqlcipher-vendored-openssl`；首次构建会编译 vendored OpenSSL（本机约 20 s），不依赖系统 openssl |
| 数据库密钥 | 钥匙串条目 `mail-client-dev / master-key`（`secret_key` 用于账户密码，两者不同）；库密钥由主密钥 HKDF-SHA256(info=`eyrie/sqlite/v1`) 派生 |
| 开发密钥存储 | 默认钥匙串（生产唯一路径）。cargo 产物是 ad-hoc 签名，钥匙串 ACL 的 partition 按 **cdhash** 记录，每次重建都重新弹窗（见 §11；自签名证书实测无效）。开发用 `--dev-secret-dir <dir>` 或 `EYRIE_DEV_SECRET_DIR` 切到 `FileStore`（目录 0700、文件 0600），主密钥与账户密码都是该目录下的文件，完全不碰钥匙串，重建后无需授权。**发布验证必须走钥匙串** |
| 网络 | 2026-10-29 实测 `imap.gmail.com:993` 直连可用（TLS 1.3 握手成功），尚未用账户登录验证；其余源码镜像可达 |
| UI 工具链 | Slint 为纯 Rust 依赖，本机可编译运行；**无 pkg-config、GTK、Qt 开发库**，GTK4/Tauri 方案在本机无法编译验证（D12 据此选定 Slint） |
| Hub 端口 | 默认标准端口，全部可覆盖：Hub API 用 `EYRIE_HUB_BIND`（代码默认 `127.0.0.1:8080`）、TLS 反代用站点配置、客户端地址自带端口；Dovecot 用 `docker -p ${DOVECOT_PORT:-31143}:143`；起服务前 `lsof -nP -iTCP:<port> -sTCP:LISTEN` 现查 |

---

## 7. 实测数据（摘要）

| 场景 | 结果 |
|---|---|
| 首次全量同步 10,002 封（release） | 1,005 ms，数据库 5.1 MB |
| 加密库全量同步 10,003 封（release，SQLCipher） | 612 ms，数据库 5.7 MB |
| 加密库空闲增量 | 1 ms（同步阶段） |
| 无密钥打开加密库 | `sqlite3` 报 `file is not a database`（错误码 26）；`strings` 搜不到主题 |
| 空闲增量 | 5 ms |
| IDLE 新邮件 → 落库 | 43 ms |
| 外部删除检测 | 69 ms（门槛化后；此前 3.4 s） |
| UIDVALIDITY 重建 | 全量 10,000 封 / 2.8 s（debug） |
| macOS 10 万行列表 | RSS 97 MB，滚动单步 p95 0.74 ms |
| UniFFI 单页 50 行调用 | 73.5 µs |
| STARTTLS（88：IMAP 143 / POP3 110 / SMTP 587） | 三类协议均升级成功并进入 AUTH；错误口令被拒 |
| POP3 同步（mock） | 2 封入库；UIDL 消失后 removed=1 |
| SMTP（mock） | AUTH PLAIN、MAIL/RCPT/DATA、点填充校验通过 |
| 邮件 HTML 净化 | 9 组攻击样本 0 失败；净化后外部请求 0；已并入 `mail-parse` 单测 |
| 正文首次拉取（multipart/alternative + 附件） | `state=partial`；附件下载后 `state=full` |
| 正文二次打开（Dovecot 已停） | 39 ms 进程整体，纯本地命中 |
| 单部件消息（`BODY.PEEK[TEXT]`） | 正常 |
| 重同步后正文缓存 | 保留，`body_state` 不回退 |
| 全账户同步（debug，Dovecot 6 邮箱） | INBOX 10,005 封 1,143 ms；Sent/项目 各 1 封 ≤14 ms；`\Sent`/`\Drafts`/`\Trash`/`\Junk` 角色正确 |
| 修改版 UTF-7 邮箱名 | LIST 的 `&mHl27g-` 解码为 `项目`；SELECT 重新编码成功 |
| 服务器删掉邮箱 | `sync` 后该行 `state=missing`，消息只在 `trash` 出现，`list` 排除 |
| 同名单重建 | 旧行改名 `项目 (local trash 2)` 保留消息；新 `项目` 全量同步，回收站不丢 |
| 回收站保留期 | 默认 90 天；`set-retention --days 30` 回读 30；未到期 `trash-purge` 返回 0 |
| 变更日志手动压缩（Dovecot 10,007 封） | 同步后 changes=10,010；压缩为快照 seq=10010，删除 10,010 行；`latest_seq` 仍为 10010，`list` 正常 |
| 变更日志自动压缩（阈值 1，注入 1 封新邮件） | `sync` 写入 2 条后自动压缩：`snapshot_seq=10012`、changes=0 |
| 变更日志 watch 路径压缩（阈值 1） | `watch --max-events 1` 首轮同步写入 2 条后压缩：`snapshot_seq=10014`、changes=0 |
| 多邮箱 watch：非 INBOX 新邮件（Dovecot 6 邮箱） | `项目` 由 IDLE 会话覆盖，投递后 ≤1 s 落库（inserted=1）；`Sent` 由扫尾连接覆盖，≤3 s（sweep=3 s） |
| 多邮箱 watch：`--max-events 2` | 收到 2 个真实变更事件后进程退出；期间扫尾空跑不写日志、不计数 |
| 多邮箱 watch：重启服务器 | 4 个 IDLE 会话同时 `connection lost` 并按 1→2→4 s 退避重连，恢复后新邮件正常入库 |
| 多邮箱 watch：邮箱被删 | 该邮箱 worker 停止，无重复报错；`sync` 将其转入回收站 |
| 全量同步 10,009 封（release，含 FTS 行重写） | 451 ms，数据库 5.79 MB（与未加正文索引时的 612 ms/5.7 MB 同级） |
| 正文索引体积（10k 封，CJK 正文约 3,800 字/封） | 仅正文缓存 124 MB；索引 1,000 字/封 +50 MB，4,000 字/封 +160 MB |
| 正文索引查询（10k 封） | 命中全表的 3 字词 ~55 ms（FTS）；中文 2 字 LIKE 52 ms（1,000 字/封）/ 137 ms（4,000 字/封）；选择性短语 0.1–0.3 ms |
| 正文搜索端到端（Dovecot） | 拉取正文前 `zebraqilin` 无结果；`body` 后命中；中文 2 字 `营收` 经 LIKE 命中 |
| schema v5→v6 迁移 | 主题索引保留、正文从 blob 回填、`user_version=6`（单测） |
| Hub `Mailbox/get`（Dovecot 10,009 封 / 7 邮箱） | 角色正确；INBOX total=10,009、unread=10,002 |
| Hub `Email/get` 按需拉 3 封未缓存正文 | 179 ms（在线 Dovecot，含 3 次 IMAP 取正文） |
| Hub `Email/changes` 增量 | 正文缓存写入后 3 条 `updated`，`hasMoreChanges=false` |
| Hub `Email/changes` 分页 | `sinceState=0, maxChanges=2` → `created=2`、`hasMoreChanges=true`、`newState=2` |
| Hub 断点续用 | Hub 重启后旧设备令牌仍有效，`last_seen_at` 更新 |
| Hub `Email/set` 置 `$seen`（Dovecot） | `updated` 返回、`oldState=10027`→`newState=10028`；服务器 `doveadm fetch` 显示 `\Seen` |
| Hub `Email/set` 清 `$seen` | 服务器 flags 为空；本地 keywords 同步清空 |
| Hub `Email/set` 移动（INBOX→归档） | 源 UID 消失、目标 `messages=1`、`Email/get` 的 `mailboxIds` 变为目标 id；INBOX 计数 10009→10008 |
| Hub `Mailbox/changes` | 同步后返回新建邮箱 `created`，`updated` 含新邮件邮箱 |
| Hub SSE | 连接即发 `state`（Email/Mailbox 同一 seq），空闲按 `ping` 发 `ping`，`closeafter=2` 后关闭 |
| Hub `/download` | 正文 blob 下载 200、`text/plain`、38 字节与 `bodyValues` 一致；未认证 401、未知 blob 404 |
| Hub `/upload` | 194/251 字节两封 `message/rfc822` 上传返回 `blobId`（sha256）；带/不带尾斜杠均 201；未认证 401；他人账户读同一 blob 404 |
| Hub `Email/import`（Dovecot） | 两条 creation 返回 `created`，`threadId` 均为 `<import-smoke-1@example.com>`；`Email/get` 的 `$seen` 与缺省关键字正确；`doveadm fetch` 确认服务器 `\Seen` 与 `In-Reply-To` 头 |
| Hub `Thread/get`（Dovecot） | 同一线程返回 `emailIds=[10021,10022]`；未知 id 进 `notFound`；`Email/changes` 把两封导入报为 `created` |
| Hub 设备吊销 | `revoke-device` 后原令牌请求 Session 返回 401 |
| Hub 上传 → 导入 → 线程（Dovecot 10,020 封库） | `/upload` 201；`Email/import` 2 封 `created`，`newState` 10033→10037；`Thread/get` 同线程 2 封；`doveadm` 头部与 `\Seen` 一致 |
| Hub 配对绑定账户（双账户库） | `POST /pair {"accountId":1}` → 201；Session `accounts` 只有 `1`；`upload/2` → 404；JMAP `Mailbox/get{accountId:2}` → `accountNotFound`，账户 1 的调用正常 |
| Hub `EmailSubmission/set`（Dovecot + 本地 SMTP sink） | 上传 180 字节 → `Email/import` 到 Drafts（id=10025）→ 提交返回 `created.k1`（`undoStatus=final`、`savedTo=Sent`、`sendAt` 时间戳）；sink 收到 3 个 `RCPT`（含 Bcc）、报文 `bcc_header=False`；Sent 6→7；`doveadm fetch` 副本无 `Bcc` 头、`\Seen` 存在；`onSuccessUpdateEmail` 产生同一 call id 的隐式 `Email/set`，本地 `$seen` 落库 |
| Hub 提交 envelope 派生 | 省略 `envelope` → 服务端按 `To` 解析出 `peer2@example.com` 并投递；`sendAt` 非空 → `notCreated.invalidProperties`；对同步来的消息（无原始报文）提交 → `notCreated.invalidEmail` |
| Hub `EmailSubmission/get` | 按 id 返回 `identityId`/`emailId`/`threadId`/`envelope`/`sendAt`/`undoStatus`/`deliveryStatus`（每个收件人 `delivered=yes`）；未知 id 进 `notFound` |
| Hub `Email/set create`（Dovecot） | 上传 175 字节 → `created.k1` 返回完整 Email 对象（`id`/`threadId`/`mailboxIds`/`keywords`/`textBody`）；`doveadm fetch` 显示 `\Seen`，消息在目标邮箱 uid=1 |
| Hub `Email/set destroy`（Dovecot） | `destroyed=["10023"]`；`Email/get` 该 id 进 `notFound`；`doveadm search` 无结果；双副本消息销毁后 INBOX/归档/Trash 均为 0 |
| Hub 多副本移动（Dovecot） | 同一 Message-ID 位于 INBOX+新箱 → `mailboxIds` 更新到归档后变 `{归档, Trash}`；服务器 归档 1 封、Trash 1 封、新箱 0 封 |
| Hub `Thread/changes`（Dovecot） | 新根 `created=["<t-root@example.com>"]`；回复并入后 `updated` 同一 id；销毁最后一份后 `destroyed` 该 id；`hasMoreChanges=false` |
| 状态按类型（单测） | mailbox 变更推进账户序列后，`Email/set` 仍接受 `Email/changes` 返回的 state（不误报 `stateMismatch`） |
| Swift FFI 冒烟（Dovecot 10,016 封） | 1 账户 / 8 邮箱 / total=10,016；首封正文 27 字；`$seen` 置清往返成功 |
| 撰写发送（CLI，Dovecot + 本地 SMTP sink） | `to/cc/bcc` → 3 个 `RCPT TO`；报文头只有 `To`/`Cc`（无 `Bcc`）；`Sent` 3→4；附件 32 字节往返一致 |
| 撰写发送（`mail-ffi` / `SiskinSmoke`） | `sendRecipients=3 savedTo=Sent saveError=-`；Sent 中可搜到主题并回读正文与附件 |
| 撰写校验 | 非法地址 → `compose: invalid address`；主题含 `\r\n` → `compose: subject contains a control character` |
| IMAP `APPEND` 标志修复 | 修复前 `APPEND "Sent" \Seen {n}` 让 Dovecot 挂起（连接一直 ESTABLISHED，`Sent` 不增长）；改为 `(\Seen)` 后立即成功，mock 回归测试断言括号形式 |
| `.app` 打包 | `dist/Siskin.app`：ad-hoc 签名、`com.eyrie.siskin`、资源 bundle 在 `Contents/Resources`；`Contents/MacOS/Siskin` 启动渲染撰写 sheet 680×560 |
| macOS 壳加载（同一库） | 窗口 1120×760；模型打印 `mailboxes=8 messages=200 total=10016`；中英文状态分别为“就绪”/“Ready” |
| macOS 阅读（id=711） | HTML 正文与附件 `note.txt` 正确渲染；附件经 FFI 下载 17 字节；操作按钮显示“标为未读 / 加星标 / 移动到” |
| macOS 快照 | `SISKIN_SNAPSHOT` 应用内渲染 2240×1520 PNG；修复前发现工具栏不可见、文字白底白字、HSplitView 底对齐、头部被英文按钮挤裁，均已修复 |
| RRULE | 22 例 21 通过，失败项为夏令时空洞时间，需归一化 |
| Hub 后台同步（Dovecot） | 不跑任何 CLI：投递一封后 ≤4 s（含 4 s 等待）`Email/query` total 10026→10027，`/healthz` 的 `sync.events` 8→9 |
| Hub `/healthz` | 匿名 200；字段 `status/database/accounts/lastSyncAt/sync{enabled,watchers,events,sweepSeconds,idleSessions}` |
| Hub 主密钥注入 | 空密钥目录 + `EYRIE_HUB_MASTER_KEY_FILE` 打开已有库：`healthz` 200、账户 1；不带 key 启动退出码 1，`database key does not match this file` |
| Hub 容器（Dockerfile） | 镜像构建成功；容器内 `/healthz` 200、`docker inspect` 显示 `healthy`；仓库外挂 `/data` + `/secrets` 后后台同步自动生效，投递一封不跑 CLI：`Email/query` total 10027→10028 |

完整方法与命令见 `docs/SPIKE-REPORT.md`。

---

## 8. 已知缺口（按优先级）

1. **不支持 Outlook/Hotmail**。2026-10-29 范围决策：微软已禁用密码登录，仅 OAuth2，且测试账号无 Entra 租户；Gmail 走应用专用密码（见 §10）。**验收**：不适用（范围外）。
2. ~~**常驻监听只覆盖单个邮箱**~~。已实现 `Core::watch_account`：可 IDLE 的邮箱（A/B 档）各占一个会话，默认 4 个；其余由扫尾连接按 `--sweep-seconds`（默认 120 s）轮询，同时重新规划邮箱集合、把消失的邮箱转回收站。Dovecot 6 邮箱实跑：非 INBOX 投递 ≤1 s、纯轮询路径 ≤sweep、重启后重连。剩余：Gmail 标签邮箱（`\All` 等虚拟箱）仍按过滤规则跳过；UI 尚未消费事件流。
3. ~~**正文未进 FTS**~~。已实现：`messages.body_text` 保存前 4,000 字纯文本，FTS 增加 `body_text` 列（schema v6，重建并从 blob 回填，打开邮件时自愈）。**已验证**：10k 封实测体积与时延见 §7；`body` 后正文与中文 2 字可搜。**未验证**：10 万封下正文 LIKE 的时延（按 10k 数据线性外推约 1.4 s，见 §8-7）。
4. ~~**变更日志线性增长**~~。已实现快照压缩：`change_snapshots` 每账户一行，超过 `change_log_max_rows`（默认 20,000）后以当前水位取全量状态并删除被覆盖的 `changes`；`latest_change_seq` 取快照 seq 兜底，`change_window` 暴露最旧可读游标。**已验证**：Dovecot 10,010 条压缩后日志归零、水位不变、`list` 正常；未验证：10 万封下快照体积与耗时。
5. **B/C 档未实跑**。**验收**：iCloud（C 档）账户实际同步，移动邮件走 COPY+删除；B 档需一台不广告 CONDSTORE 的服务器。
6. ~~**重连未做故障注入**~~。已在 `watch_account` 下实测：4 个 IDLE 会话在 Dovecot 重启时同时断线，按 1/2/4 s 退避重连，恢复后新邮件正常入库且无重复行。未覆盖：网络分区（连接挂起而非断开）与「重连后服务端丢了 UIDVALIDITY」之外的乱序响应。
7. **中文 1–2 字搜索走 LIKE 扫描**，现在同时扫正文列。10k 封（正文约 3,800 字/封）实测 2 字查询 137 ms；10 万封按线性外推约 1.4 s，**未验证**。**验收**：10 万封下 2 字查询 P95 < 150 ms（需要专门的分词/前缀索引，不能靠 LIKE）。
8. **Windows/移动端零验证**。**验收**：WinUI 3 调 Rust（uniffi-bindgen-cs），空闲内存 ≤ 160 MB。
9. **POP3 语义受限**。无文件夹、无标志位、无 IDLE；本地删除仅影响本地，服务器删除通过 UIDL 列表消失检测；UIDL 以 63 位哈希映射为数字 uid。**验收**：真实 POP3 账户同步与删除行为符合预期。
10. **发信能力窄**。仅纯文本 UTF-8；无附件、HTML、草稿；Sent 归档仅在 IMAP 账户尝试。**验收**：真实账户收到邮件，Sent 归档或明确报错。
11. **明文端口需显式开关**。`--security plaintext` 或 `--smtp-security plaintext` 必须同时给 `--insecure-plaintext true`；QQ 的 143/110 不提供 STARTTLS。
12. **主密钥无轮换与自动备份**。已提供 `export-key`/`import-key` 做人工备份（可存 Bitwarden 等），但主密钥仍只有钥匙串一份，丢失且无备份即数据不可恢复；库密钥无法在线轮换。**验收**：轮换后旧库可迁移；备份丢失时给出明确提示。
13. **回收站只覆盖“整箱消失”**。单封邮件在服务器端消失仍按 UID 差集硬删除（正常流程应先进入服务器 Trash 并同步到本地 Trash）；回收站不提供手工恢复。**验收**：UI 可按邮箱/时间浏览回收站；恢复操作有明确语义。
14. **多邮箱列表已去重**。同 Message-ID 只展示一个代表位置；按邮箱筛选、跨邮箱计数需要新的查询接口。**验收**：列表可按邮箱过滤且不重复，计数与筛选一致。
15. **macOS 通知、草稿与多账户切换未做**。列表/阅读/搜索/同步/标已读/标星/移动/HTML/附件/分页/i18n/撰写（To/Cc/Bcc + 附件）/`.app` 打包已接通。剩余：新邮件通知（需 FFI 事件流 + `UNUserNotificationCenter`）、草稿保存、真实多账户切换。**验收**：新邮件可触发系统通知；**未验证**：真实 Gmail/QQ/88 账户投递（§10），通知权限与签名分发。
16. **Hub 写操作覆盖面**。已完成 `keywords`/`mailboxIds` 更新、多副本移动、`/upload`、`Email/import`、`Email/set` 的 `create`/`destroy`、`Thread/get|changes`、`Identity/get`、`EmailSubmission/get|set`（create）；自定义关键字、APNs/FCM、`Email/query` 的 `operator`/`conditions` 组合过滤与升序排序、`EmailSubmission` 的 update/destroy/delayed send 未做；导入/创建只支持 IMAP 账户与单邮箱；一次 `Email/get` 内正文串行拉取；`Thread/changes` 只覆盖本构建之后的变更（升级前历史线程不补发，客户端先 `Thread/get`）。**验收**：客户端可导入、按线程展示、永久删除并发送（已完成）；APNs/FCM 待凭据。
17. **UIDVALIDITY 重建不逐封报 destroyed**。`reset_mailbox` 不写逐消息变更，JMAP 客户端依赖重新全量加载；正常增量删除已覆盖。**验收**：重建后客户端能收到 `cannotCalculateChanges` 或完整 destroyed 列表。
18. **客户端双模式未实现（D13，下一项）**。Hub 侧前置已完成：设备令牌绑定账户、标准 `EmailSubmission/set` 发送、后台同步（IDLE 池 + 扫尾）、`/healthz` 与容器镜像。剩余前置：对外 HTTPS 依赖反代（按 `docs/ARCHITECTURE.md` §3.2）。客户端适配层（模式选择、地址 + 令牌配置、JMAP 客户端、独立缓存与清除）未开始。**验收**：同一 `.app` 首次运行可选两种模式；远程模式在断网时仍能打开上次缓存的列表，清缓存后可重新加载；客户端不保存邮箱密码。
19. ~~**Hub 部署底线未补**~~。已补：进程内后台同步（每账户 `watch_account`，`EYRIE_HUB_SYNC`/`EYRIE_HUB_SWEEP_SECONDS`/`EYRIE_HUB_IDLE_SESSIONS`）、匿名 `GET /healthz`（库不可读 503）、仓库根 `Dockerfile`（多阶段、非 root UID 10001、`HEALTHCHECK`）与 `.dockerignore`、文件密钥后端 `EYRIE_HUB_SECRET_DIR` 及主密钥 `EYRIE_HUB_MASTER_KEY_FILE` 注入（缺 key/错 key 启动即失败）。**未做**：TLS 仍由反代终止；快照备份脚本。**验收结果**：Dovecot 实跑不跑 CLI 时投递自动入库（10026→10027）；容器内同样自动同步（10027→10028）、`/healthz` 200 且 `HEALTHCHECK` 为 healthy；缺 key 启动失败。

---

## 9. 下一步路线（按顺序，独立可交付）

### 第 2 段：安全与认证（完成）

- SQLCipher 加密库完成；OAuth2 按范围决策不做（见 §3 与 §8-1）。

### 第 3 段：内容能力（完成）

2. ~~正文与附件按需拉取；接入 `spike/html-probe` 的净化规则。~~（完成）
3. ~~多邮箱遍历与角色映射。~~（完成；含修改版 UTF-7 与本地回收站）
4. ~~变更日志压缩与快照。~~（完成）
5. ~~多邮箱实时监听（IDLE 池 + 扫尾）与正文全文索引。~~（完成；见 §3/§7；正文前 4,000 字，中文 2 字仍走 LIKE）

### 第 4 段：服务端与移动（进行中）

6. Hub（axum + JMAP 子集 + 设备配对 + APNs/FCM）。**读写子集、SSE、附件下载、设备吊销、上传、`Email/import`、`Email/set create/destroy`、多副本移动、`Thread/get|changes`、令牌绑定账户、`Identity/get`、`EmailSubmission/get|set`、后台同步（IDLE 池 + 扫尾）、`/healthz`、容器镜像与密钥后端完成**（2026-11-06）；剩余：APNs/FCM 推送（缺凭据）、TLS 反代与备份脚本。
7. iOS/Android 薄客户端（依赖 Hub 推送凭据）。

### 第 5 段：日历与翻译

8. EventKit / CalendarContract 桥 + CalDAV/Google/Graph；邀请函。
9. 翻译管线（本地/自托管/云）。

### 第 6 段：桌面 UI

10. 客户端双模式（D13）：首次运行选择本地/远程；远程填服务端地址 + 设备令牌（配对时绑定账户）；抽象 UI 数据源（FFI 本地 / JMAP 远程）；独立缓存与清缓存。远程发送已就绪（标准 `EmailSubmission/set`），客户端需在 FFI 侧暴露「按 compose 字段生成 RFC 822 + envelope」的助手，避免 Swift 再写一套 MIME。
11. macOS SwiftUI：列表/阅读/搜索/同步/标已读/撰写（To/Cc/Bcc + 附件）/`.app` 打包完成；剩余通知、草稿、真实多账户切换。
12. Windows WinUI 3（复用 Rust 核心）。
13. Linux Slint（`apps/linux`，workspace 成员，单进程调用 `mail-core`；先做列表/阅读/搜索，再补撰写与设置）。栈已定，实现未开始。

### 远程模式部署分布（D13，推荐）

- 拓扑：客户端 → TLS 反代 → Hub 单进程（JMAP/SSE + 内核 + 同步任务）→ SQLCipher 库；Hub 出站连 IMAP/POP3/SMTP。
- 每用户一个 Hub 实例、一个库、一个持久卷；主密钥与库分开放、分开备份；容器内用 KMS/文件密钥后端，不依赖系统钥匙串。
- 桌面用 SSE；手机后续由 Hub 转 APNs/FCM（载荷只含"有新变更"）。
- 端口：默认使用标准端口，且全部可通过变量/配置覆盖——Hub API `EYRIE_HUB_BIND`、TLS 反代站点配置、客户端地址自带端口、Dovecot `-p`、开发 SMTP sink 环境变量；默认端口被占用时用覆盖值（如 18080/18443），不改代码常量，部署前用 `lsof` 现查。
- 上线前置：~~Hub 内置同步任务~~、~~服务端密钥后端~~、~~令牌绑定账户~~、~~发送提交端点~~、~~`/healthz`~~、~~Hub 镜像~~（均已完成）；剩余 TLS（反代终止）、DB 快照备份、APNs/FCM 凭据。
- 详细原则、场景表与上线清单见 `docs/ARCHITECTURE.md` §3.2。

---

## 10. 待外部输入（没有则无法推进）

| 项 | 用途 | 影响 |
|---|---|---|
| 一个 Gmail 测试账户（应用专用密码，需开启 2FA 与 IMAP） | Gmail 能力与标签映射验证 | 未提供则 Gmail 路径未实测 |
| 一个 QQ 或 88 测试账户（授权码/专用密码） | 真实预设同步 + POP3/IMAP/SMTP 验收 | 未提供则真实收发未实测 |
| 一个 iCloud 账户（应用专用密码） | C 档与 COPY+删除路径实测 | 阻塞 §8-5 的 C 档验证 |
| 一台 Windows 11 机器 + .NET SDK | WinUI 3 + UniFFI 内存验证 | 阻塞 Windows UI 段 |
| APNs / FCM 凭据 | 手机推送 | 阻塞移动端 |
| 自建 Hub 的域名与 TLS 证书（或明确只在内网/回环使用） | 远程模式连通与安全验收 | 未提供则远程模式只能本机回环验证 |
| ~~是否先做 Hub 还是先做桌面 UI~~ | 排序 | 已定：先做 Hub（第 4 段），桌面 UI 顺延 |

---

## 11. 风险与陷阱（已踩过，勿重犯）

- `keyring` 缺平台特性会静默不存，必须检查实际钥匙串。
- SQLCipher 原始密钥只能写成 `PRAGMA key = "x'<hex>'"`；`PRAGMA key = x'<hex>'`（裸 blob 字面量）是语法错误。
- 主密钥条目名 `master-key` 与账户密码条目 `account:<email>:password` 不同；`MASTER_KEY_REF` 或 HKDF info（`eyrie/sqlite/v1`）一旦变更，既有库永久打不开。
- 旧明文库必须显式拒绝：`StoreError::PlaintextDatabase`。迁移只能 `sqlcipher_export` 或重新同步，不能原地改。
- 验收加密库不要用 `sqlite3 file '.tables'`（对非库文件可能退出 0 且无输出）；用 `sqlite3 file 'select count(*) from sqlite_master;'` 看错误码 26。
- 重建 `mail-cli` 后首次访问钥匙串会弹出授权框，点“始终允许”也不跨版本生效。根因：cargo 产物是 **ad-hoc 签名**（`Signature=adhoc`，即使 Identifier 稳定），macOS 对 ad-hoc 二进制的钥匙串 ACL 绑的是 CDHash，每次重建 CDHash 都变；`target/debug` 与 `target/release` 还是两个不同身份。已实测证伪的“修复”：用自签名证书重签并固定 `-i com.eyrie.mail-cli` **无效**——无 Team ID 时钥匙串 partition 仍落在 cdhash 上，`securityd` 记 `ACL partition mismatch: client cdhash:… ACL (cdhash:…)`，每次重建照样弹窗（2026-10-30 实测，debug 与 release 各弹一次）。能真正稳定 partition 的只有 Apple Developer ID（带 Team ID）签名。开发时用 `--dev-secret-dir` / `EYRIE_DEV_SECRET_DIR` 切到文件存储即可绕开（2026-10-30 实测：重编后 0.25 s 完成取密钥，无弹窗）；无 TTY 的自动化冒烟走 `MemoryStore` 单测路径，发布验证必须回到钥匙串，不要依赖已授权的 GUI 交互。
- 切换密钥存储不会迁移主密钥：`--dev-secret-dir` 下没有钥匙串里的那把密钥，会新生成一把；用它打开钥匙串加密的旧库会报 `WrongKey`。要换存储先 `export-key`，再用 `import-key --value '<base64>'` 导入，或直接用新库重新同步。
- `export-key` 的标准输出就是主密钥本身：只让用户复制进密码管理器，不要进 shell 历史、日志、提交或聊天记录；调试时比对 sha256 前缀。
- `import-key` 必须先验证再落库：库存在且导入密钥打不开它时直接失败，不覆盖钥匙串；只有 `--force true` 才替换已有的不同密钥。
- FTS5 `contentless_delete=1` 必须同时写 `content=''`，否则建表失败。
- trigram 分词器不支持 <3 字符查询，中文需 LIKE 回退。
- Gmail 应用专用密码要求账户开启两步验证；Gmail 的 IMAP 还需在设置中显式启用，未启用时认证会失败但密码本身有效。
- QQ 的 143/110 可连但不广告 STARTTLS（STLS），走这两个端口等于明文传授权码；默认应使用 993/995。
- 88（Coremail）不广告 IDLE/CONDSTORE，档位为 C（轮询）；`imap/pop/smtp` 主机按域名区分（88.com / 111.com / email.cn）。
- POP3 无 UIDL 持久化列：UIDL 经 SHA-256 截断为 63 位数字 uid；容器内冲突概率可忽略，但不要把它当协议 UID 展示。
- SMTP 发送后 IMAP 追加 Sent 失败不回滚投递，只返回 `save_error`；POP3 账户没有 Sent 归档。
- `async-imap::Session::append` 把 `flags` 原样拼进命令：传 `\Seen` 会发出 `APPEND "Sent" \Seen {n}`，Dovecot 不回 continuation，客户端永久挂起（连接 ESTABLISHED、`Sent` 不增长、没有任何报错）。`mail-proto::ImapSession::append` 已负责包成 `(\Seen)`；改这里别再拆掉括号，回归测试断言命令文本。
- 撰写报文的 Bcc 只能进 SMTP 信封（`RCPT TO`），绝不能写 `Bcc:` 头；`compose::compose` 返回的 `envelope` 是去重后的 to+cc+bcc，SMTP 与头字段必须用这一份。
- 地址只接受 `local@domain`：带显示名（`Name <a@b>`）、非 ASCII（SMTPUTF8）与含 `,;:<>` 的串都会被拒绝；主题/附件名中的控制字符同样拒绝（头注入）。UI 侧按 `,`/`;` 拆地址，不要在 core 里放宽。
- 非 ASCII 主题按 45 字节切成多段 RFC 2047 encoded-word 并折叠；附件名用单段 encoded-word（`mail-parse::clean_filename` 能解回）。不要改用 RFC 2231：解析端还不支持。
- 附件合计上限 25 MiB，超限返回 `ComposeError::AttachmentsTooLarge`，不做截断；MIME 类型由 Swift `UTType.preferredMIMEType` 提供，非法或缺失回退 `application/octet-stream`。
- 预设 provider 的 SMTP 缺省由 `Core::add_account` 补（465/TLS），`custom` 必须显式携带；`mail-ffi::NewAccountView` 的 SMTP 字段为空时不要伪造 host。
- `.app` 里的 SwiftPM 资源必须放在 `Contents/Resources/Siskin_SiskinApp.bundle`：`Bundle.module` 只查 `Bundle.main.resourceURL` 等固定位置，漏拷会让 `L10n` 退回 key 原文。
- `apps/macos/dist/` 是生成物（gitignored）；`package.sh` 用 release Rust + release Swift，别拿 `build.sh` 的 debug 产物冒充分发包。
- iCloud 无 IDLE/CONDSTORE/MOVE；每次全量 UID 扫描代价高（10 万封时不可接受），必须保留“数量变化/超时”门槛。
- `UIDVALIDITY` 变化必须整箱重建，不能复用 UID。
- 草稿与已发送的去重尚未实现，接入发送功能时先解决。
- `BODY.PEEK` 与 `BODY` 的差别是是否置 `\Seen`：取正文一律用 `PEEK`，否则打开邮件等于标记已读。
- IMAP 分段号只对 multipart 有意义：单部件消息取 `BODY[TEXT]`，不要拼 `[1]`；`fetch_part` 用空 part 表示这一分支。
- POP3 的 `multiline` 曾把 `..dotted` 裁成 `dotted`（多删一个点），已修正为只去掉一个前导点；任何按行剥点的代码都要先过 `RETR` 单测。
- blob 引用计数只在“新引用”时 +1：重复存同一份正文必须命中同一 hash 并跳过计数，否则 `reset_mailbox` 后残留。
- POP3 的正文部件键是 `text`/`html`，不是 IMAP section；不要把它当 UID 或 section 复用。
- 附件先落 `attachments` 元数据（含传输编码）、下载后回填 `blob_hash`；漏存 encoding 会导致 base64 附件写盘为编码串。
- 净化用 ammonia 的 `attribute_filter`，不要回到 regex 后处理：`url(...)`、`javascript:`、远程 `src` 都要按属性过滤，正则易被引号与实体绕过。
- Mailspring-Sync 为 GPL-3，可与 AGPL 组合，但不要无意引入其他不兼容许可。
- IMAP 邮箱名线上是修改版 UTF-7：`LIST` 返回 `&mHl27g-`、`SELECT` 要用同一编码；对外 API 统一 UTF-8（`mail-proto::utf7`），不要在两处各写一套编解码。
- `\All`/`\Flagged` 是虚拟邮箱：Gmail 的 All Mail 会聚合其他邮箱，同步它会让每封邮件多一份位置；`\Noselect` 只是层级占位，SELECT 会失败。
- `mark_missing_mailboxes` 必须把 INBOX 当永远存在：LIST 响应异常缺失时不能把主邮箱打进回收站。
- 同名单重建（新 UIDVALIDITY）先 `retire_mailbox` 再 `ensure_mailbox`，否则 `reset_mailbox` 会立即清掉回收站里的旧消息。
- 多邮箱下同 Message-ID 只有一条 `messages` 行、多条 `message_locations`；列表/搜索/计数必须取一个代表位置（`missing_at IS NULL` 的最小 `mailbox_id`），否则 INBOX 与 Archive 重复。
- 回收站清理顺序：先释放 blob 引用、再删 FTS 行、最后删消息；漏删 FTS 会留下无法 join 的孤儿行。
- `settings` 表是全局配置（不是 account.settings）；保留期键 `trash_retention_days`，默认 90，范围 1–3650；变更日志阈值键 `change_log_max_rows`，默认 20,000，范围 1–1,000,000。
- 变更日志水位不要再用 `SELECT MAX(seq) FROM changes`：压缩后日志可以为空，必须取 `max(snapshot.seq, max(changes.seq))`（`latest_change_seq` 已封装，直接用）。
- 压缩的顺序是“同一事务内取状态 → 写快照 → 删被覆盖的行”；先删后取会把已删的变更丢掉，取状态与写快照不在同一事务会让快照与实际状态错位。
- `change_snapshots` 每账户只有一行，再次压缩会覆盖旧快照；要保留历史快照必须先加版本列，不要直接改主键。
- 快照只含 id/位置/标志位，不含正文散列与 FTS：它用于重建“有哪些邮件在哪”，正文仍按需拉取。
- Linux UI 用 Slint（D12）：本机无 pkg-config / GTK / Qt 开发库，GTK4 与 Tauri 方案在此既不能编译也不能验证；Slint 是纯 Rust 依赖，可直接在本机编译运行同一份代码。
- `watch` 现在只把「有新增/删除/重建」的结果当事件，扫尾空跑不回调、不消耗 `--max-events`：因为周期性的全量标志位重扫会把每行都算成 `applied`，无法与真实标志变化区分。只看标志位变化的消费者走变更日志。
- IDLE 会话数默认 4（`--idle-sessions`，0 即纯轮询），加上 1 条扫尾连接；Gmail 每账户上限 15 条 IMAP 连接，别为「每箱一个会话」直接按邮箱数开。
- 停止 watcher 会直接丢弃正在 IDLE 的连接（`IdleWait` 已把会话取出，无法退回），服务端看到的是 TCP 断开而非 LOGOUT；只在退出路径上发生，不要试图在此加 `logout`。
- 扫尾连接必须跳过已有 IDLE 会话的邮箱（`sync_account_except`），否则同一邮箱会被两条连接同时同步；但跳过列表仍要参与 `mark_missing_mailboxes`，否则消失的邮箱进不了回收站。
- FTS5 不能 `ALTER TABLE … ADD COLUMN`；加列必须删表重建。`message_fts` 的 DDL 只放在 `FTS_SCHEMA` 一处，建库与迁移共用，避免两份定义漂移。
- `message_fts` 是 contentless（`content=''`），**不能 UPDATE**：改一条消息的索引必须 `DELETE` 再 `INSERT`（`rewrite_fts`）；也因此拿不到 `snippet()`，列表里的摘要仍用 `messages.snippet`。
- 千万别在 `SCHEMA` 的 `CREATE TABLE` 里写 `--` 注释：迁移测试要 `ALTER TABLE … DROP COLUMN` 时，SQLite 会按存储的原始 SQL 重解析表定义并报 `incomplete input`。
- 正文索引只存前 4,000 字（`BODY_INDEX_CHARS`）：超出部分搜不到是设计取舍，实测体积/时延见 §7；改上限要重跑 10k 基准并同步本文件数字。
- 老库升级时正文只能从 `mail_parts` 的纯文本部件回填；HTML 邮件要等用户打开一次（`fetch_body` 命中缓存后 `index_body_text_if_missing`）才进索引。
- 中文 1–2 字查询走 LIKE，现在也扫 `body_text`：10k 实测 137 ms，10 万封会明显超预算，扩大索引前先解决分词，不要指望 LIKE。
- 用 `doveadm save` 投递测试邮件必须 `docker exec -i`；漏掉 `-i` 会读到 EOF，存进去的是空邮件（主题/正文全空，UID 照常增长，很容易误判成解析 bug）。
- JMAP 的 `Email/changes` 依赖 change 的 `op` 语义：`apply_fetch` 现在写 `message created/updated/removed`；旧库里的 `upsert` 按 `updated` 处理。创建后又在同一窗口删除的 id 两个列表都不出现，创建后又更新的 id 只进 `created`。
- 读正文会写 `message updated` 变更：`Email/get` 拉取正文后 `state` 会前进，客户端会看到该邮件一次 `updated`。这是有意为之，不要当成重复变更过滤掉。
- `Email/changes` 的游标判断是三条：`since > latest_seq`、`snapshot_seq > since`、`since < oldest_seq - 1`，任一成立都必须 `cannotCalculateChanges`；只看 `oldest_seq` 会漏掉“压缩后日志为空”的情况。
- `mailbox_counts` / 关键词过滤用 `json_each(flags)` 精确匹配 `\Seen`/`\Flagged`；不要换成 `flags LIKE '%\Seen%'`，自定义关键字会被误命中。
- 单部件邮件的正文 section 是空字符串（`BODY[TEXT]`）；JMAP 的 `partId` 不能为空，映射为 `text`/`html`，`bodyValues` 也用同一键，否则客户端按 `partId` 取不到值。
- `messages_with_locations` 只返回 `missing_at IS NULL` 的位置；回收站里的邮箱不能出现在 JMAP `mailboxIds` 或 `Mailbox/get` 里。
- Hub 设备令牌明文只在 `/pair` 响应里出现一次；库里只有 SHA-256 哈希。配对码未配置时每进程随机生成并打印到 stdout，不要把它写进日志或提交。
- macOS 的 `Bundle.module.preferredLocalizations` 在本机返回 `["en"]`（系统却是 zh-Hans-CN），不能靠 `NSLocalizedString(key, bundle: .module)` 自动选语言；`L10n` 直接按 `Locale.preferredLanguages`/`SISKIN_LANG` 读取 `.lproj/Localizable.strings`。
- RFC 2047 编码主题/发件人必须在 `mail-sync::to_new_message` 解码（新数据）并在 `mail-ffi::message_view` 再解一次（旧库已存原始串）；不要在 Swift 里另写一套解码。
- SwiftUI `List(selection:)` 在 `NSHostingView` 里会触发 “reentrant operation in its NSTableView delegate”，消息列表与侧栏改为 `ScrollView + LazyVStack` 后消失；不要再把两侧改回 `List(selection:)`。
- `NSHostingView` 根视图不自动画窗口背景：深色模式下文字是白色、背景是白色，必须给根视图 `.background(Color(nsColor: .windowBackgroundColor))` 并设置 `window.backgroundColor`。
- `HSplitView` 子视图要显式 `.frame(maxHeight: .infinity)`，否则列会按内容高度贴底，顶部留大片空白。
- 详情页操作行比面板宽时，整个 `VStack` 会被撑宽并左右裁切（中英文都出现过）：把 HTML/纯文本切换移到独立一行，并给头部 `.frame(maxWidth: .infinity, alignment: .leading)`。
- `NSHostingView` 会在首次布局时把窗口缩到根视图最小尺寸；设置 `window.setContentSize` 要在启动后延迟一次再设，或同时设置根视图目标尺寸。
- `mail-ffi` 的公开类型一改，`apps/macos/Generated` 与 `Sources/mail_ffiFFI` 就是旧签名：必须重跑 `apps/macos/build.sh`，不要手工改生成文件。
- SwiftPM 的 system library 必须叫 `module.modulemap`（生成文件叫 `mail_ffiFFI.modulemap`），且 C 头与 modulemap 要放在 `Sources/mail_ffiFFI/`；`Generated/` 只放 Swift 绑定，否则 `import mail_ffiFFI` 找不到模块。
- 静态链接 Rust 产物会出现 “built for newer macOS version (27.0)” 警告：Rust 按本机 SDK 编译，SwiftPM 目标写 macOS 14；只影响链接告警，不影响运行，别用 `-w` 掩盖真正错误。
- `Siskin` 平时用 SwiftPM 可执行文件 + AppKit 引导启动；`package.sh` 才产出 `.app`。两条路径都不带环境变量时都会走钥匙串，ad-hoc 签名每次重建都会弹授权；开发与冒烟一律显式设置 `EYRIE_DEV_SECRET_DIR`。
- 只跑 `cargo clippy` 不会更新 `target/debug/mail-hub`；改完 `mail-api` 后要 `cargo build -p mail-hub` 再重启，否则冒烟仍在跑旧逻辑（本次实测踩过）。
- IMAP 标志位按副本存：JMAP keyword 更新必须遍历 `message_location_targets` 的每个在线位置，全部 `STORE` 成功后才写本地 flags；中间失败就让本地保持旧值，等下一次同步收敛（`update_flags` 只更新本地，不碰服务器）。
- `WriteSink::set_keywords` 的入参是 JMAP keyword（`$seen`），映射成 IMAP flag（`\Seen`）是 `mail-core::keyword_flag` 的职责；测试里的假 sink 也要映射，否则会存进 `$seen` 这种非 IMAP 标志。
- 移动只接受单副本消息：多个位置意味着服务器上有多份，IMAP `MOVE` 只能动一份，直接返回 `notSupported`。目标 UID 不靠 `COPYUID`（async-imap 丢弃响应码），而是移动后同步目标邮箱取得新位置，再删源位置；目标同步没看到新邮件时不要删源位置。
- `message_location_targets` 只返回 `missing_at IS NULL` 的邮箱；回收站里的副本不参与写操作，否则会 SELECT 一个服务器上已不存在的邮箱。
- `blob_for_account` 必须是账户作用域：只按 `blobId` 取字节会让任意已配对设备读到别的账户缓存；`blobId` 是 32 字节散列的 hex，解码失败按 404 处理。
- SSE 的 `state` 事件每个账户一条，`Email` 与 `Mailbox` 共用同一个账户水位（seq）；`closeafter`/`ping` 同时接受纯秒数与 `PTxxS`，不要只解析一种。
- `ensure_mailbox` 现在会写 `mailbox created/restored`，所以变更日志计数比旧版本多一条（新库首次建 INBOX 即有）；按实体类型取增量必须用 `changes_since_type`，不能用 `changes_since` 再在内存过滤，否则分页 `hasMoreChanges` 会被其他类型的行干扰。
- `record_change` 接收 `&Connection`，`Transaction` 通过 Deref 直接传入；不要在事务里另开连接写日志，否则会与 SQLite 单写者锁冲突。
- `thread_id` 只在插入时确定：回复先到时先以自身 `Message-ID` 为根，父到达后再由 `adopt_descendants` 的递归 CTE 改挂整棵子树；不要在查询时临时计算线程，否则 `Email/get.threadId` 与 `Thread/get` 会不一致。
- `in_reply_to` 按协议原样存：IMAP ENVELOPE 带尖括号，POP3 分支去掉了尖括号。父查找同时尝试原值与 `<原值>`，不要为统一格式去改 `message_id`（会让旧库同一封邮件重复插行）。
- 上传的引用计数：同一内容再次上传不 `+1`；`uploads` 到期只释放它自己那一份，`message_parts`/`attachments` 持有各自的引用。
- Session 的 `uploadUrl` 模板带尾斜杠，axum 不做尾斜杠重定向，`/upload/{accountId}` 与 `/upload/{accountId}/` 必须同时注册。
- `Email/import` 必须 APPEND 后再同步该邮箱才能拿到本地行；优先按 `Message-ID` 定位，没有才用 UID 差集，差集多于一条时返回 `ImportNotLocated` 而不是猜。
- JMAP `Email/set destroy` 是**永久删除**（RFC 8621 §4.6）：对每个活动副本 `STORE \Deleted` + `UID EXPUNGE`，再 `remove_all_locations` 清掉本地回收站里的副本。删除到 Trash 是 `mailboxIds` 更新，不是 destroy；不要为了“安全”把 destroy 改成移入 Trash，那会让客户端“清空回收站”永远清不掉。
- 多副本移动必须把多余副本移入 `\Trash` 角色邮箱（`mailbox_by_role`），目标只保留一份；没有 Trash 邮箱时返回 `notSupported`，绝不要 MOVE 每个副本到目标（会在服务器上复制出 N 份）。
- 线程变更由写侧记录，不要在读了消息后再推导：`created` 的判定必须发生在 `adopt_descendants` **之前**，否则晚到的父邮件会把 3 个被收养的回复算进来，误判成 `updated`。
- `purge_orphan_messages` 已经负责为被清理的消息写 `thread updated/removed`；`reset_mailbox`、`purge_trash`、`remove_location`、apply_fetch 的删除路径都复用它，别在调用处再补一遍。
- JMAP state 按实体类型取水位（`latest_change_seq_type`）：mailbox/thread 行会推进账户级序列，若 `Email/set` 用 `latest_change_seq` 校验 `ifInState`，客户端刚拿到的 `Email/changes` state 会被误判成 `stateMismatch`。
- `Thread/changes` 只记录本构建之后的线程变更；旧库升级后历史线程没有 `changes` 行，客户端应先用 `Thread/get`。压缩会同时删掉 thread 行，早于快照的游标一律 `cannotCalculateChanges`。
- 删除最后一份副本时 `remove_all_locations` 必须先删 `message_locations` 再 `purge_orphan_messages`；顺序反了会留下没有位置的消息行（`Email/get` 看不到但线程仍把它算进 `emailIds`）。
- 远程模式客户端只能持有 Hub 地址与设备令牌，**不得保存邮箱密码**，否则等于绕开服务端、破坏“凭据只在服务端”的边界；本地模式的账号与钥匙串也不能被远程模式复用。
- Hub 的令牌必须绑定账户：`hub_devices.account_id` 为 NULL 时一律 403 要求重新配对，绝不回退成“全部账户”。迁移只在库里恰好一个账户时回填；多账户旧库保持未绑定。
- 账户隔离要在服务端每个入口校验：Session 只列绑定账户、`/upload`/`/download`/SSE 只认绑定账户、JMAP 每个方法调用的 `accountId` 必须等于绑定账户（否则该调用返回 `accountNotFound`）。不要相信客户端自报的账户。
- 提交只接受 `Email/import` 或 `Email/set create` 写入的消息：原始报文存在 `messages.raw_blob_hash`，同步来的邮件没有 raw，提交返回 `invalidEmail`。不要为了“能发”从服务器重新下载并重解析。
- `attach_raw_blob` 复用 `store_blob` 的引用计数：同一内容重复挂接不 `+1`，不同内容先释放旧 hash；原始报文的引用在 `purge_orphan_messages` 的 blob UNION 里释放，不要在别处二次释放。
- `EmailSubmission` 走客户端给的信封：`Bcc` 只出现在 `envelope.rcptTo`，投递前用 `compose::strip_bcc_header` 从报文头里删掉；不要用 `compose()` 重新生成报文，否则客户端上传的字节被替换。
- 省略 `envelope` 时服务端按 `To`/`Cc`/`Bcc` 头推导（`compose::derive_envelope`），推导失败按 `notSupported` 报错；`sendAt` 非空必须拒绝（`maxDelayedSend=0`）。
- `onSuccessUpdateEmail` 是隐式 `Email/set`：必须排在 `EmailSubmission/set` 响应之后，且两条响应共用同一个方法 call id。
- 发送成功但 Sent 归档失败只体现在 `saveError`/`savedTo`；Sent 的服务端副本要等下一次 `sync` 才进本地库，提交后立刻 `Email/get` 可能只看到 Drafts 位置（本地 `send` 同样如此）。
- `mailboxIds/<id>: null` 与 `mailboxIds/<id>: true` 同时出现表示 JMAP 的移动，后端用 IMAP `MOVE` 满足；只删不加、或删除多个邮箱都不支持，返回 `notSupported`。
- `EmailSubmission` 没有变更日志：`oldState`/`newState` 取该账户最新提交 id（`e<id>`）；不要把提交写入 `changes`，否则 `Email/changes` 的游标语义会被污染。
- Hub 默认仍是明文 HTTP、绑定 `127.0.0.1:8080`；后台同步已内置（默认开启），TLS 仍由反代终止。远程模式对外开放前必须有 TLS 入口；容器内绑定 `0.0.0.0` 只代表容器网络，不要直接把 8080 暴露到公网。端口默认用标准值，但必须能通过 `EYRIE_HUB_BIND`、反代配置或客户端地址覆盖，不要在代码里改常量。
- Hub 的密钥目录（`EYRIE_HUB_SECRET_DIR`，含主密钥与账户密码）与数据库分开挂卷；主密钥文件经 `EYRIE_HUB_MASTER_KEY_FILE` 注入时，必须挂只读。缺 key 或错 key 会让启动直接失败（`database key does not match`），不要为了“能启动”删库重建。
- 远程模式的“清除缓存”只清客户端缓存与对应 state，绝不调用服务端删除；清完后从 `state=0` 重新拉当前窗口，不能把 `cannotCalculateChanges` 当成清缓存。
- 列表允许全量元数据缓存，正文/附件按需；不要为了“列表本地”在客户端预先拉全量正文，也不要为了“不全量”把列表改成每次都走网络。

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
