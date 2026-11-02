# 实现说明（同步内核 + 加密库 + 多协议 + 正文索引 + 多邮箱实时监听）

日期：2026-10-30
范围：工程骨架、本地存储（SQLCipher 加密）、凭据保险箱、IMAP/POP3 接收、SMTP 发送、提供商预设、正文/附件按需拉取与 HTML 净化、正文全文索引（schema v6）、全账户多邮箱遍历与实时监听、本地回收站与保留期、开发 CLI
状态：协议路径已实测（Dovecot + 88 服务器 STARTTLS）；正文与附件、多邮箱、回收站、实时监听、正文搜索对 Dovecot 实跑；真实账号收发未验收

---

## 1. 已交付

| crate | 职责 | 关键点 |
|---|---|---|
| `mail-crypto` | 凭据保险箱 + 信封加密 | 系统钥匙串（macOS Keychain / Windows Credential Manager / Linux Secret Service）；XChaCha20-Poly1305 带 AAD；主密钥首次使用时生成并存入钥匙串，HKDF-SHA256 派生用途子密钥 |
| `mail-store` | SQLite 存储（SQLCipher） | 账户、邮箱、消息、位置、变更日志、FTS5（主题/地址/摘要/正文）；内容寻址 `blobs` + 引用计数；`message_parts`/`attachments` 与 `body_state`；`body_text` 存前 4,000 字纯文本，FTS 行用 `rewrite_fts` 删+插重建（contentless 不能 UPDATE）；变更日志有界（`change_snapshots` 全量快照 + 压缩，水位不倒退）；WAL；schema v6 迁移（重建 FTS 并从 blob 回填正文）；UIDVALIDITY 重建；邮箱 `missing_at` 本地回收站与保留期清理；`settings` 表；打开时先验密钥，错密钥与明文旧库都显式报错 |
| `mail-parse` | MIME 解码与净化 | 传输编码（base64/quoted-printable）、charset（encoding_rs，含 GBK）、RFC 2047 文件名、ammonia 白名单净化、HTML→纯文本、整封 MIME 遍历（POP3 路径） |
| `mail-proto` | IMAP/POP3/SMTP 传输 | IMAP：TLS/STARTTLS/明文、能力探测、三档、UID 抓取、BODYSTRUCTURE + `BODY.PEEK` 分段取正文、IDLE、MOVE、RFC 6154 SPECIAL-USE、修改版 UTF-7 邮箱名；POP3：LIST/UIDL/TOP/RETR、STLS；SMTP：EHLO、STARTTLS、AUTH、DATA |
| `mail-sync` | 增量同步 | IMAP：首同步、UID/标志位增量、删除检测、UIDVALIDITY 重建、全账户遍历（跳过 `\Noselect`/`\All`/`\Flagged`）、消失邮箱转本地回收站；POP3：UIDL 镜像与轮询 |
| `mail-core` | 门面 | 账户添加、预设解析、IMAP/POP3 分派、`sync`/`sync_account`、`watch`（单邮箱）/`watch_account`（全邮箱：IDLE 池 + 扫尾连接，只推送真实变更；每轮同步后按阈值压缩变更日志）、`check`、`send`（SMTP + Sent 归档）、`fetch_body`（缓存优先，命中时补写正文索引）、`fetch_attachment`（按需）、回收站保留期设置与自动清理、`change_window`/`compact_changes` |
| `mail-cli` | 开发工具 | `add-account / check / send / accounts / sync / watch / mailboxes / list / search / trash / trash-purge / body / attachment / stats / retention / set-retention / changes / changes-compact / set-change-max-rows / export-key / import-key` |

产品 UI、Hub、日历、翻译不在本段范围内。

---

## 2. 同步流程

```mermaid
flowchart TD
  A[连接 + 能力探测] --> B{UIDVALIDITY 变化?}
  B -- 是 --> C[清空该邮箱缓存]
  B -- 否 --> D{首次同步?}
  C --> E[全量抓取 1:*]
  D -- 是 --> E
  D -- 否 --> F[抓取 uidnext:*]
  F --> G{A 档且有 modseq?}
  G -- 是 --> H[CHANGEDSINCE 抓标志位]
  G -- 否 --> I{标志位同步到期?}
  I -- 是 --> J[全量抓标志位]
  I -- 否 --> K[跳过]
  H --> L{消息数不符或对账到期?}
  J --> L
  K --> L
  L -- 是 --> M[UID SEARCH ALL 求差集]
  L -- 否 --> N[跳过]
  E --> O[写入 SQLite + 变更日志]
  M --> O
  N --> O
  O --> P[更新 uidnext / modseq / 时间戳]
```

删除检测不再每次全量扫描：只有服务端 `EXISTS` 与本地条数不一致，或距上次对账超过 15 分钟，才执行 `UID SEARCH ALL`。实测把一次删除同步从 3.4 s 降到 69 ms。

### 2.1 常驻监听（`watch_account`）

```mermaid
flowchart LR
  S[扫尾连接<br/>每 120 s] -->|LIST + 重规划| P[邮箱计划]
  P -->|A/B 档且有空位| W[IDLE 会话池 ≤4]
  S -->|同步未覆盖邮箱<br/>mark_missing| DB[(SQLite)]
  W -->|新数据即增量同步| DB
  W --> E[事件汇: 仅新增/删除/重建]
  S --> E
```

- A/B 档（广告 IDLE）的邮箱各占一条连接，按 `plan_mailboxes` 顺序取前 `idle_sessions` 个（默认 4，`--idle-sessions 0` 即纯轮询）。
- 其余邮箱由扫尾连接轮询（`--sweep-seconds`，默认 120 s）；扫尾同时重新 LIST，新增邮箱补进计划、消失邮箱停掉 worker 并进回收站。
- 扫尾用 `sync_account_except` 跳过已有会话的邮箱，避免同一邮箱双写；跳过列表仍参与 `mark_missing_mailboxes`。
- 事件只在 `inserted + removed > 0` 或 `full`（新邮箱/UIDVALIDITY 重建）时发出：周期性全量标志位重扫会把每行都算成 `applied`，无法与真实变化区分。POP3 保持 60 s 轮询，同样只回调真实变更。
- 故障：连接断开按 1→2→4 s 退避（上限 60 s）重连；退出时直接丢弃 IDLE 连接（IDLE 中的会话无法退回）。

---

## 3. 协议、端口与安全

| 服务 | IMAP TLS | IMAP STARTTLS | POP3 TLS | POP3 STLS | SMTP TLS | SMTP STARTTLS |
|---|---|---|---|---|---|---|
| Gmail | 993 | 143 | 995 | 110 | 465 | 587 / 25 |
| QQ | 993 | 143（服务器不提供 STARTTLS） | 995 | 110（无 STLS） | 465 | 587 / 25 |
| 88 / 111 / email.cn | 993 | 143 | 995 | 110 | 465 | 587 / 25 |
| 自定义 | 用户指定 | 用户指定 | 用户指定 | 用户指定 | 用户指定 | 用户指定 |

- `--provider gmail|qq|88|custom` 决定主机；`--protocol imap|pop3` 决定接收协议；`--security tls|starttls|plaintext` 决定加密方式；`--port/--smtp-port` 可覆盖默认端口。
- 默认端口由协议与安全方式推导（IMAP 993/143、POP3 995/110、SMTP 465/587）。
- 明文必须显式开启：`--security plaintext` 或 `--smtp-security plaintext` 需要同时提供 `--insecure-plaintext true`，否则拒绝写入账户。
- QQ 的 143/110 不提供 STARTTLS/STLS，使用这两个端口即明文传授权码；文档与错误信息会提示改用 993/995。
- 发信为纯文本 UTF-8（base64 正文）；IMAP 账户发送成功后尝试追加到 `Sent`（`--sent-mailbox` 可改），失败只报告 `save_error`，不回滚投递；POP3 无 Sent 归档。

---

## 4. 实测结果（Dovecot 2.3.19，10,000 封）

| 场景 | 结果 |
|---|---|
| 首次全量同步（release） | 10,002 封 / **1,005 ms**，数据库 5.1 MB |
| 加密库首次全量同步（release） | 10,003 封 / **612 ms**，数据库 5.7 MB |
| 加密库空闲增量 | **1 ms**（同步阶段） |
| 无密钥打开 | `sqlite3 … 'select count(*) from sqlite_master;'` → `file is not a database`（26）；`strings` 无主题命中 |
| 错密钥打开 | `StoreError::WrongKey`（单测覆盖） |
| 明文旧库打开 | `StoreError::PlaintextDatabase`（单测覆盖；CLI 实跑同结果） |
| 空闲增量（release） | **5 ms**（进程整体 38 ms） |
| 新邮件经 IDLE 到达并同步 | **43 ms**（含通知与写入） |
| 标志位变更（CHANGEDSINCE） | applied=1，无重复插入 |
| 外部删除一封 | removed=1，**69 ms** |
| UIDVALIDITY 重建 | 清空后全量 10,000 封 / 2,794 ms（debug 构建） |
| STARTTLS（88：IMAP 143 / POP3 110 / SMTP 587） | 升级后进入 AUTH，错误口令被拒 |
| POP3 同步（mock） | 2 封入库；UIDL 消失后 removed=1 |
| SMTP（mock） | AUTH PLAIN、MAIL/RCPT/DATA、点填充校验通过 |
| 正文拉取（multipart/alternative + 附件，Dovecot） | 首次取正文 1 次连接，`state=partial`；附件下载后 `state=full` |
| 正文二次打开（服务器已停） | **39 ms** 进程整体，纯本地缓存命中 |
| 单部件消息（`BODY.PEEK[TEXT]`） | 正常取回，`state=full` |
| 附件按需下载（base64） | 解码后内容与原文一致；`blob_bytes` = 正文 + HTML + 附件之和 |
| 重同步后正文缓存 | 保留，`body_state` 不回退 |
| 全账户同步（debug，Dovecot） | LIST 6 个邮箱；INBOX 10,005 封 1,143 ms，Sent/项目 各 1 封 ≤14 ms；`\Sent`/`\Drafts`/`\Trash`/`\Junk` 角色映射正确 |
| 修改版 UTF-7 邮箱名 | LIST 的 `&mHl27g-` 解码为 `项目`；SELECT 时重新编码，同步成功 |
| 服务器删除邮箱 | 删 `项目` 后 sync：该行 `state=missing`，消息只在 `trash` 出现，`list` 不再返回 |
| 同名单重建 | 旧行改名为 `项目 (local trash 2)` 并保留消息，新 `项目` 全量同步；回收站内容未丢 |
| 保留期设置 | 默认 90 天；`set-retention --days 30` 后 `retention` 回读 30；`trash-purge` 未到期返回 0 |
| 多邮箱 watch（Dovecot 6 邮箱） | 非 INBOX（`项目`，IDLE 覆盖）投递后 ≤1 s 入库；`Sent`（扫尾覆盖，sweep 3 s）≤3 s；`--max-events 2` 收到 2 个真实事件后退出 |
| 多邮箱 watch：服务器重启 | 4 个 IDLE 会话同时断线，按 1/2/4 s 退避重连；恢复后新邮件正常入库 |
| 多邮箱 watch：邮箱删除 | worker 停止，无重复报错；该邮箱 `state=missing` |
| 全量同步（release，含 FTS 行重写） | 10,009 封 / **451 ms**，数据库 5.79 MB |
| 正文索引体积（10k 封，CJK 正文 ~3,800 字/封） | 仅正文缓存 124 MB；1,000 字/封索引 +50 MB；4,000 字/封 +160 MB |
| 正文索引查询（10k 封） | 命中全表的 3 字词 ~55 ms；中文 2 字 LIKE 52 ms（1,000 字/封）/ 137 ms（4,000 字/封）；选择性短语 0.1–0.3 ms |
| 正文搜索（Dovecot 端到端） | 拉取前搜 `zebraqilin` 无结果，`body` 后命中；中文 2 字 `营收` 经 LIKE 命中 |
| schema v5→v6 迁移 | 主题索引保留、正文从 blob 回填、`user_version=6`（单测） |
| 单测 | 92 个全绿（crypto/store/sync/proto/parse/core） |

首次同步后进程启动有一次钥匙串授权检查（新二进制），之后的同步都在 40 ms 内。

---

## 5. 正文与附件

- 同步阶段只写头部，`body_state='headers'`；打开邮件时才拉正文，二次打开只读本地库。
- IMAP：`FETCH (UID BODYSTRUCTURE)` → 叶子部件按 `1`、`2.1` 编号 → 只对正文部件发 `BODY.PEEK[<section>]`（不置 `\Seen`）；附件留到用户点下载。
- POP3：无分段，`RETR` 整封 → `mail-parse` 遍历 MIME → 正文与附件一次入库。POP3 的正文部件键固定为 `text`/`html`，不伪装成 IMAP section。
- 解码：`mail-parse` 处理传输编码与 charset（含 GBK）；HTML 经 ammonia 白名单净化，`script/iframe/form/style` 移除，`url(...)` 与 `javascript:` 丢弃，远程图片改 `about:blank`，只放行 `cid:` 与光栅 data URI。
- 存储：`blobs` 按 sha256 内容寻址 + 引用计数；`message_parts` 存已解码 UTF-8 正文；`attachments` 存元数据（含传输编码），下载后回填 `blob_hash`；`body_state` 由 `refresh_body_state` 统一推导：`headers` → `partial` → `full`。
- 附件文件名统一取 basename、剥离控制字符、解码 RFC 2047、截断 255 字符；不自动打开（隔离目录导出待 UI 段）。

---

## 5.1 多邮箱遍历与本地回收站

- 遍历：`sync` 不带 `--mailbox` 时对 IMAP 账户执行 `LIST "" "*"`，跳过 `\Noselect`（层级占位）与虚拟邮箱 `\All`/`\Flagged`（Gmail 会聚合其他邮箱，同步会重复）；INBOX 永远排在最前，即使 LIST 缺失也会补同步。
- 角色：优先 RFC 6154 属性（`\Sent`/`\Drafts`/`\Trash`/`\Junk`/`\Archive`），没有属性才按名字回退（含 `INBOX.Sent`、`[Gmail]/Sent Mail` 与中文“已发送/草稿/已删除/垃圾邮件”）。角色只写 `mailboxes.role`，不改远程名。
- 名称编码：IMAP 线上邮箱名是修改版 UTF-7；`list_mailboxes` 解码成 UTF-8，`select`/`append`/`move_message` 负责重新编码。`INBOX` 是纯 ASCII，不受影响。
- 去重：同一封（Message-ID 相同）在多个邮箱里只保留一个 `messages` 行与多条 `message_locations`；`list/search/count` 取一个代表位置（`missing_at IS NULL` 的最小 `mailbox_id`），避免 INBOX 与 Archive 重复展示。
- 回收站：服务器不再列出的邮箱写 `mailboxes.missing_at`，其消息从正常列表、搜索、计数中隐藏，只在 `list_trash` 出现；取正文时优先仍在服务器上的位置。
- 保留期：`settings.trash_retention_days` 默认 90 天（1–3650 可设）；每次 `sync`/`sync_account` 先清理超过保留期的回收站，删除邮箱行、无位置的消息、正文/附件 blob 与 FTS 行。
- 同名单重建：删掉后又被重建的邮箱通常是新 UIDVALIDITY；先把旧行改名为 `<name> (local trash <id>)` 留在回收站，再给新同名邮箱建新行，避免重建时提前删掉旧消息。
- 单封邮件在服务器端消失仍按 UID 差集硬删除（服务器侧通常已移入 Trash 文件夹，随后同步到本地 Trash）；回收站保留期只作用于“整个邮箱消失”的数据。

---

## 6. 运行方式

macOS 上 `cargo run` 产出的二进制是 ad-hoc 签名，钥匙串 ACL 按 cdhash 记录授权，**每次重建后首次访问都会弹授权框**，点“始终允许”也只对当次产物有效；自签名证书同样无效（无 Team ID，partition 仍是 cdhash，详见 `docs/HANDOFF.md` §11）。开发时显式切到文件存储即可绕开钥匙串：

```bash
export EYRIE_DEV_SECRET_DIR=/tmp/eyrie-dev-secrets   # 目录 0700、文件 0600；仅开发
```

也可以用 `--dev-secret-dir <dir>` 逐条命令指定。发布验证和真实用户路径必须不设置它（默认走钥匙串）；自动化冒烟仍走 `Store::open_in_memory` + `MemoryStore` 测试路径。

```bash
# 1) 测试服务器（脚本见 docs/SPIKE-REPORT.md）
docker start mail-spike-dovecot

# 2) 测试账户（Dovecot 明文，必须显式允许）
cargo run -p mail-cli -- --db /tmp/mail.db add-account \
  --email vmail@example.com --host 127.0.0.1 --port 31143 \
  --security plaintext --insecure-plaintext true \
  --username vmail --password testpass

# 3) 提供商预设（密码/授权码从 stdin 读）
printf 'app password: ' >&2
read -rs APP_PW
printf '%s\n' "$APP_PW" | cargo run -p mail-cli -- --db /tmp/mail.db add-account \
  --provider gmail --email you@gmail.com --security tls --password-stdin
unset APP_PW

# QQ：授权码；88：专用密码；自定义：显式 --host/--smtp-host
# 接收协议可选 imap/pop3；安全方式 tls/starttls/plaintext

# 4) 验证登录与发信
cargo run -p mail-cli -- --db /tmp/mail.db check --email you@gmail.com
cargo run -p mail-cli -- --db /tmp/mail.db send --email you@gmail.com \
  --to peer@example.com --subject hello --body 'plain text body'

# 5) 同步与查看（不带 --mailbox 同步该账户全部可选邮箱）
cargo run -p mail-cli -- --db /tmp/mail.db sync --email vmail@example.com
cargo run -p mail-cli -- --db /tmp/mail.db sync --email vmail@example.com --mailbox INBOX
cargo run -p mail-cli -- --db /tmp/mail.db mailboxes --email vmail@example.com
cargo run -p mail-cli -- --db /tmp/mail.db stats --email vmail@example.com
cargo run -p mail-cli -- --db /tmp/mail.db list --email vmail@example.com --limit 20

# 5.0) 本地回收站：服务器删掉的邮箱进回收站；保留期默认 90 天
cargo run -p mail-cli -- --db /tmp/mail.db trash --email vmail@example.com
cargo run -p mail-cli -- --db /tmp/mail.db retention
cargo run -p mail-cli -- --db /tmp/mail.db set-retention --days 30
cargo run -p mail-cli -- --db /tmp/mail.db trash-purge

# 5.1) 正文与附件（id 取自 list 的 id= 列；--html true 输出净化后的 HTML）
cargo run -p mail-cli -- --db /tmp/mail.db body --email vmail@example.com --id 42
cargo run -p mail-cli -- --db /tmp/mail.db attachment --email vmail@example.com \
  --id 42 --part 2 --out /tmp/report.pdf

# 5.2) 搜索：主题/地址/摘要 + 已拉取正文的前 4,000 字；中文 1–2 字走 LIKE
cargo run -p mail-cli -- --db /tmp/mail.db search --email vmail@example.com --query 营收

# 6) 常驻监听：省略 --mailbox 覆盖全部可选邮箱（IDLE 池 4 + 扫尾 120 s）
cargo run -p mail-cli -- --db /tmp/mail.db watch --email vmail@example.com
cargo run -p mail-cli -- --db /tmp/mail.db watch --email vmail@example.com \
  --idle-sessions 0 --sweep-seconds 2   # 纯轮询路径
cargo run -p mail-cli -- --db /tmp/mail.db watch --email vmail@example.com --mailbox INBOX
# 事件只在有新增/删除/重建时触发；投递测试邮件要带 docker exec -i，否则存入空邮件

# 7) 验证加密：首次打开会自动在主密钥条目下生成密钥
sqlite3 /tmp/mail.db 'select count(*) from sqlite_master;'   # 期望 file is not a database
strings /tmp/mail.db | grep -c 'Subject'                     # 期望 0

# 8) 主密钥备份 / 恢复（输出即密钥本身，存进密码管理器）
cargo run -p mail-cli -- --db /tmp/mail.db export-key
cargo run -p mail-cli -- --db /tmp/mail.db import-key --value '<base64>'          # 相同则不改动
cargo run -p mail-cli -- --db /tmp/mail.db import-key --value '<base64>' --force true
# import-key 会用 --db 指向的库验证密钥：打不开就失败，不覆盖钥匙串

# 9) 测试
cargo test --workspace
```

---

## 7. 已知缺口（按优先级）

1. **仅支持密码/应用专用密码认证**。Outlook 需 OAuth2 应用身份，按 2026-10-29 范围决策不接；Gmail 走应用专用密码（待真实账户验证）。
2. ~~**常驻监听仅单邮箱**~~。`watch` 省略 `--mailbox` 时走 `watch_account`：A/B 档邮箱各一条 IDLE 会话（默认 4 条），其余由扫尾连接轮询（默认 120 s），并重新规划邮箱集合。未覆盖：Gmail 虚拟邮箱（`\All`/`\Flagged`）按规则跳过；UI 尚未消费事件流。
3. ~~**正文未进 FTS**~~。`messages.body_text` 保存前 4,000 字纯文本并进 `message_fts`（schema v6，重建迁移 + 打开邮件自愈）。未验证：10 万封下正文 LIKE 的时延。
4. **B/C 档未实跑**。代码路径已实现，但没有 iCloud 等 C 档测试账户；能力矩阵来自未登录探测。
5. ~~**变更日志会持续增长**~~。已实现快照压缩：超过 `change_log_max_rows`（默认 20,000）后写 `change_snapshots` 并删除被覆盖的 `changes` 行，水位由快照 seq 兜底；`change_window` 暴露最旧可读游标。剩余缺口只在 Hub/JMAP 侧：`cannotCalculateChanges` 尚未有 HTTP 端点消费该信号。
6. **中文 1–2 字搜索走 LIKE 扫描**，现在也扫 `body_text`。10k 封实测 137 ms；10 万封按线性外推 ~1.4 s（未验证），更大规模需要专门的分词索引。
7. ~~**重连退避未做故障注入**~~。`watch_account` 下实测 Dovecot 重启：IDLE 会话同时断线并按退避重连，恢复后正常入库。未覆盖网络分区（连接挂起）等更隐蔽的故障。
8. **POP3 语义受限**。无文件夹/标志位/IDLE；UIDL 以 63 位哈希映射为数字 uid；服务器删除通过 UIDL 消失检测。
9. **发信能力窄**。仅纯文本；无附件、HTML、草稿；Sent 归档只在 IMAP 尝试，失败仅告警。
10. **真实账号未验收**。QQ/88/Gmail 的同步与发信需要授权码/专用密码。
11. **主密钥无轮换与自动备份**。`export-key`/`import-key` 支持人工备份到密码管理器，但主密钥仍只有钥匙串一份，且库密钥不能在线轮换。
12. **回收站只覆盖“整箱消失”**。单封邮件在服务器端消失仍按 UID 差集硬删除（正常流程应已进入服务器 Trash 并同步到本地）；回收站不提供手工恢复，同名单重建时旧数据以 `(local trash <id>)` 行保留至保留期。
13. **多邮箱会让 `list/search` 去重**。同 Message-ID 只展示一个代表位置；按邮箱筛选的 UI 需要新的查询接口。

---

## 8. 下一段建议

按依赖顺序：

1. 发信补齐：HTML、附件、草稿与 Sent 去重
2. ~~FTS 正文索引（`body_text`）~~（完成；中文 1–2 字仍走 LIKE，10 万封前需要分词方案）
3. ~~多邮箱实时监听（每邮箱一个 IDLE 会话或轮询调度）~~（完成；IDLE 池 + 扫尾）
4. 主密钥导出/恢复与库密钥轮换
