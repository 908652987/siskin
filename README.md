# Siskin

统一收件箱与日历客户端。多邮箱、多日历、按需翻译，桌面可单机运行，也可连接自建同步服务器。

- 许可：AGPL-3.0（见 `LICENSE`）
- 状态：**同步内核已实现并实测通过**；UI、日历、Hub 尚未开始
- 目标平台：macOS、Windows、iOS、Android

## 现在能做什么

- 用系统钥匙串保存邮箱密码，数据库只存引用
- IMAP 全量同步、增量同步、IDLE 实时监听、删除检测、UIDVALIDITY 重建
- 本地 SQLite 存储与全文索引（含变更日志，供后续多端同步）
- 三档同步策略，覆盖支持/不支持 QRESYNC、IDLE 的服务器

当前没有图形界面，验证入口是开发用命令行工具 `mail-cli`。

## 快速开始

```bash
# 依赖：Rust stable、Docker（仅测试用）
cargo build --release

# 测试服务器（生成 1 万封测试邮件）
python3 spike/scripts/gen_mail.py --root spike/data/Maildir --count 10000
docker build -t mail-spike-dovecot spike/dovecot
docker run -d --name mail-spike-dovecot -p 31143:143 \
  -v "$PWD/spike/data/Maildir:/home/vmail/Maildir" mail-spike-dovecot
docker exec mail-spike-dovecot doveadm index -u vmail INBOX

# 添加账户并同步
cargo run -p mail-cli -- --db /tmp/mail.db add-account \
  --email vmail@example.com --host 127.0.0.1 --port 31143 --tls false \
  --username vmail --password testpass
cargo run -p mail-cli -- --db /tmp/mail.db sync --email vmail@example.com
cargo run -p mail-cli -- --db /tmp/mail.db list --email vmail@example.com
cargo run -p mail-cli -- --db /tmp/mail.db watch --email vmail@example.com
```

## 仓库结构

| 路径 | 内容 |
|---|---|
| `crates/mail-crypto` | 凭据保险箱、XChaCha20-Poly1305 信封加密 |
| `crates/mail-store` | SQLite schema、变更日志、FTS5 |
| `crates/mail-proto` | IMAP/TLS、能力探测、三档同步原语 |
| `crates/mail-sync` | 增量同步引擎 |
| `crates/mail-core` | 门面与常驻 IDLE 循环 |
| `crates/mail-cli` | 开发命令行 |
| `spike/` | 开工前验证工程（IMAP 压测、RRULE、HTML 净化、Swift 基准） |
| `docs/` | 架构、验证报告、实现说明、交接存档 |

## 文档

- `docs/ARCHITECTURE.md` — 设计定稿：模块边界、数据模型、协议、安全基线、性能预算
- `docs/SPIKE-REPORT.md` — 五项验证的实测数据与结论
- `docs/IMPLEMENTATION.md` — 第一段实现说明、运行方式、已知缺口
- `docs/HANDOFF.md` — **从这里继续任务**：进度、环境、下一步、待决问题

## 测试

```bash
cargo test --workspace
```

需要 Dovecot 的端到端步骤见 `docs/IMPLEMENTATION.md`。
