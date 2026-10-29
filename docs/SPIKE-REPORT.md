# 开工前验证报告

日期：2026-10-28
环境：macOS 27.0.1 (arm64)、Rust 1.99.1、Swift 6.4、Dovecot 2.3.19.1（Docker）、Python 3 + dateutil 2.9
范围：S1 IMAP、S2 macOS 列表、S3 UniFFI、S4 HTML 安全、S5 日历重复规则

**未使用任何真实邮箱凭据**；真实提供商只做了未登录的能力探测。

---

## 1. 结论

| 项 | 结果 | 关键数据 |
|---|---|---|
| S1 IMAP 同步 | **通过（有条件）** | 1 万封头部同步 1.10 s（9,116 封/秒），峰值内存 7.0 MB，IDLE 通知 1.32 s |
| S2 macOS 十万封列表 | **通过** | 10 万行 RSS 97 MB，滚动单步 p95 0.74 ms，建窗口 28.6 ms |
| S3 UniFFI 跨语言 | **部分通过** | Swift 侧 50 行/次调用 73.5 µs；Kotlin 绑定可生成；**Windows 未验证** |
| S4 邮件 HTML 安全 | **通过（需一条修正）** | 9 组攻击样本 0 失败；净化后外部请求 0，未净化页面会加载追踪像素 |
| S5 日历重复规则 | **通过（需一条修正）** | 22 例对 dateutil 21 通过，1 例为夏令时空洞时间 |

结论：核心假设成立，可以进入实现。但 S1 暴露了一个必须改架构的事实——**不是所有邮箱都支持增量同步扩展**。

---

## 2. S1：IMAP 能力与性能

### 2.1 本地 Dovecot（1 万封，全功能服务器）

| 指标 | 结果 |
|---|---|
| 能力 | QRESYNC、CONDSTORE、IDLE、MOVE、UIDPLUS、SPECIAL-USE、ENABLE、SORT |
| SELECT INBOX | 66 ms |
| UID FETCH 1:*（UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE） | 10,000 封 / 1,097 ms（9,116 封/秒） |
| 峰值 RSS | 7.0 MB |
| CHANGEDSINCE 增量 | 10,000 条标志变更 / 17 ms |
| IDLE 通知延迟 | 追加后 1,320 ms（含 APPEND 往返；追加动作在 800 ms 触发） |

### 2.2 真实提供商能力矩阵（未登录探测，仅问候语中的 CAPABILITY）

| 提供商 | IDLE | CONDSTORE | QRESYNC | MOVE | 认证方式 | 备注 |
|---|---|---|---|---|---|---|
| Outlook / M365 | ✅ | ❌ | ❌ | ✅ | 仅 XOAUTH2（LOGINDISABLED） | 必须 OAuth2 |
| iCloud | ❌ | ❌ | ❌ | ❌ | PLAIN / XOAUTH2 / ATOKEN | 有 XAPPLEPUSHSERVICE |
| Fastmail | ✅ | ✅ | ✅ | ✅ | PLAIN / XOAUTH2 | 标准实现 |
| Yahoo | ✅ | ❌ | ❌ | ✅ | PLAIN / XOAUTH2 / OAUTHBEARER | 私有 XYMHIGHESTMODSEQ |
| Gmail | 未探测到 | 未探测到 | 未探测到 | 未探测到 | — | 本机网络到 imap.gmail.com:993 TLS 被中断 |

### 2.3 对架构的影响（必须改）

原设计假设“支持 CONDSTORE 就用 CHANGEDSINCE，否则 UID 范围增量”。真实矩阵要求**三档同步策略**：

1. **A 档（Fastmail、Dovecot、Cyrus）**：QRESYNC + CONDSTORE，用 `CHANGEDSINCE` + `VANISHED`。
2. **B 档（Outlook、Yahoo）**：无 CONDSTORE。用 `UID FETCH <last+1>:*` 拉新邮件，标志位靠周期性全量对账（建议 15 分钟）+ `NOOP` 触发变更。
3. **C 档（iCloud）**：无 IDLE、无 CONDSTORE、无 MOVE。只能用 `STATUS`/`NOOP` 轮询（60 s 起），移动邮件必须用 COPY + STORE \Deleted + EXPUNGE。若要实时，需要实现 Apple 私有 `XAPPLEPUSHSERVICE`（需 APNs 证书，暂不纳入）。

移动端依赖 Hub 推送，因此 C 档的轮询频率只影响 Hub 侧成本，不影响手机耗电。

---

## 3. S2：macOS 十万封列表

方法：AppKit `NSTableView`（行复用）+ 10 万行内存模型，程序化滚动 300 步，每步 24 pt。

| 指标 | 结果 |
|---|---|
| 模型构建（10 万行） | 56 ms |
| 窗口显示 + reloadData | 28.6 ms |
| RSS（仅模型） | 49.0 MB |
| RSS（含窗口） | 96.9 MB |
| 滚动单步 p50 / p95 / max | 0.21 / 0.74 / 9.62 ms |
| 抽样 `rect(ofRow:)` 1,031 次 | 0.068 ms |

说明：这是“布局 + 绘制派发”耗时，不是 vsync 帧率；`fps_equivalent` 只能作为裕量参考。结论是 10 万行有充足余量，预算 120 MB 可以守住。

---

## 4. S3：UniFFI

| 指标 | 结果 |
|---|---|
| Swift 调用（50 行/页） | 73.5 µs/次 |
| Swift 调用（空字符串往返） | 0.07 µs/次 |
| 25 万行跨语言传输 | 368 ms |
| 进程 RSS | 6.5 MB |
| Kotlin 绑定 | 生成成功（未接 Android 工程编译） |
| C# / WinUI | **未验证**：本机无 dotnet SDK，也没有 Windows 主机 |

结论：FFI 单页调用比 1 ms 预算低一个数量级，分页 50 条的设计成立。Windows 侧仍需在真机上验证 `uniffi-bindgen-cs` + WinUI 3 的链接与内存。

---

## 5. S4：邮件 HTML 安全

### 5.1 净化器（9 组攻击样本）

样本：内联脚本、`<img onerror>`、`javascript:` 链接、iframe+表单、CSS `url()` 外带、SVG 脚本、data URI SVG、追踪像素表格、普通邮件。

结果：全部通过，脚本/事件处理器/iframe/表单/JS URL/CSS url() 均被清除；远程图片（含**相对路径**）被改写为 `about:blank`。

### 5.2 真实 WKWebView 行为（JS 关闭）

| 页面 | 服务器收到的请求 |
|---|---|
| 未净化 `raw.html`（含 `/pixel.png` 与 `fetch('/beacon')`） | `/raw.html`、`/pixel.png`；**无** `/beacon` |
| 净化后 `clean.html` | 仅 `/clean.html`；无任何图片请求 |

结论：
1. 关闭 JS 能阻止脚本外带，但**不会阻止远程图片**——净化器是必需的，不是可选的。
2. 净化器原实现只拦 `http(s)://` 绝对地址，**漏掉了相对路径**；已修正为“只允许 `cid:` 与光栅 data URI，其余一律阻断”。
3. 单个 WKWebView 实例常驻约 72.8 MB，必须按消息池化并在切换时释放。

---

## 6. S5：日历重复规则

22 个用例（含 BYSETPOS、BYWEEKNO、月末、闰日、WKST、跨夏令时），与 python-dateutil 逐条比对：21 通过，1 失败。

失败用例：`DTSTART;TZID=America/New_York:20240310T020000`。2024-03-10 02:00 在纽约不存在（夏令时跳过）。`rrule` 直接报 `InvalidDateTimeInLocalTimezone`，dateutil 会归一化到 03:00。

修正：解析 `.ics` 后、调用重复规则前，先做**本地时间归一化**（不存在的时间向后推一个 DST 间隔；重复的时间取第一次出现）。这个逻辑必须由我们实现并在写入前执行，否则用户在其他客户端创建的“夏令时空洞”事件会导致同步失败。

---

## 7. 复现命令

```bash
# 依赖
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
# 国内网络需要 ~/.cargo/config.toml 指向镜像（本机使用 rsproxy）

# 测试邮件与服务器
python3 spike/scripts/gen_mail.py --root spike/data/Maildir --count 10000
docker build -t mail-spike-dovecot spike/dovecot
docker run -d --name mail-spike-dovecot -p 31143:143 \
  -v "$PWD/spike/data/Maildir:/home/vmail/Maildir" mail-spike-dovecot
docker exec mail-spike-dovecot doveadm index -u vmail INBOX

# S1
cargo run --release -p imap-probe -- caps
/usr/bin/time -l cargo run --release -p imap-probe -- fetch
cargo run --release -p imap-probe -- condstore
cargo run --release -p imap-probe -- idle --timeout 15

# S5
python3 spike/scripts/rrule_cases.py spike/data/rrule-cases.json
cargo run --release -p rrule-probe -- spike/data/rrule-cases.json > spike/data/rrule-rust.json
python3 spike/scripts/compare_rrule.py spike/data/rrule-cases.json spike/data/rrule-rust.json

# S4
cargo run --release -p html-probe
swiftc -O -framework AppKit -framework WebKit spike/swift/WebViewProbe.swift -o spike/swift/webview-probe
python3 spike/scripts/http_probe_server.py spike/data/web spike/data/web-requests.log 8765 &
./spike/swift/webview-probe http://127.0.0.1:8765/raw.html
./spike/swift/webview-probe http://127.0.0.1:8765/clean.html

# S2
swiftc -O -framework AppKit spike/swift/ListBench.swift -o spike/swift/listbench
./spike/swift/listbench 100000

# S3
cargo build --release -p ffi-probe
cargo run --release --bin uniffi-bindgen -- generate \
  --library target/release/libffi_probe.dylib --language swift --out-dir spike/ffi-probe/gen
```

---

## 8. 未验证项

| 项 | 原因 | 需要的条件 |
|---|---|---|
| Gmail 能力与同步 | 本机到 `imap.gmail.com:993` 的 TLS 被中断 | 换网络或代理后重跑 `probe_providers.py` |
| 真实账户的 UIDVALIDITY 重建、草稿去重、Gmail 标签 | 无凭据 | 一个测试邮箱 + 应用专用密码 |
| Windows WinUI 3 + UniFFI + 内存 | 无 Windows 主机与 dotnet SDK | Windows 11 开发机 |
| Android/iOS 后台与推送 | 未搭移动工程 | 两台真机 + APNs/FCM 凭据 |
| 日历 CalDAV/Graph 实际同步 | 未接真实账户 | 测试日历账户 |

---

## 9. 对实现计划的修正

1. 同步层从“两档”改为“三档能力矩阵”，并把能力探测结果缓存到账户配置。
2. iCloud 账户默认 60 s 轮询，UI 明确标注“非实时”；移动端仍由 Hub 推送。
3. 净化器规则改为“默认全部阻断，仅放行 `cid:` 与光栅 data URI”；补相对路径与 `srcset`。
4. `.ics` 解析增加 DST 空洞时间归一化。
5. Windows 实施方案保持不变，但把 WinUI 3 + UniFFI 的验证前置为 Windows 开工第一件事。
