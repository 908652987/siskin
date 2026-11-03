# Siskin macOS 壳

SwiftUI 单窗口客户端，通过 `mail-ffi`（UniFFI）直连 Rust 核心。当前覆盖账户选择/
新增（Gmail/QQ/88/自定义预设）、邮箱与邮件列表、HTML（JS 禁用）+ 纯文本阅读、搜索、同步、标已读/未读、标星、移动、分页加载、附件保存，以及 zh-Hans/en 国际化；撰写、通知与真正的 `.app` 打包尚未接入。

## 构建

```bash
./apps/macos/build.sh
```

脚本会：编译 `mail-ffi` → 重新生成 Swift 绑定与 C 头 → 复制静态库 →
`swift build`。生成物（`Generated/`、`libs/`、`Sources/mail_ffiFFI/`、`.build/`）
不入库。

## 运行

```bash
SISKIN_DB=/tmp/mail.db EYRIE_DEV_SECRET_DIR=/tmp/eyrie-dev-secrets \
  apps/macos/.build/debug/Siskin
```

`SISKIN_DB` 省略时使用 `~/Library/Application Support/Siskin/mail.db`；也可以把库路径
作为第一个参数传入。`EYRIE_DEV_SECRET_DIR` 仅用于开发：不设置时走系统钥匙串。
`SISKIN_LANG=en` 可强制英文（默认跟随系统语言）。

窗口快照（无需屏幕录制权限）：

```bash
SISKIN_DB=/tmp/mail.db EYRIE_DEV_SECRET_DIR=/tmp/eyrie-dev-secrets \
  SISKIN_SNAPSHOT=/tmp/ui.png SISKIN_SNAPSHOT_MESSAGE=711 \
  apps/macos/.build/debug/Siskin
```

无界面冒烟（列表、正文、关键字往返）：

```bash
SISKIN_DB=/tmp/mail.db EYRIE_DEV_SECRET_DIR=/tmp/eyrie-dev-secrets \
  apps/macos/.build/debug/SiskinSmoke
```

## 注意

- 改动 `mail-ffi` 的公开类型或方法后必须重跑 `build.sh`，否则 Swift 绑定仍是旧签名。
- 链接静态库时出现 “built for newer macOS version” 警告：Rust 产物按本机 SDK 构建，
  SwiftPM 目标写的是 macOS 14；当前只影响警告，不影响运行。
