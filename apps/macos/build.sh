#!/usr/bin/env bash
# Builds the Rust FFI library, regenerates Swift bindings, and compiles the app.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
app="$root/apps/macos"
cargo="${CARGO:-$HOME/.cargo/bin/cargo}"

"$cargo" build -p mail-ffi
mkdir -p "$app/Generated" "$app/Sources/mail_ffiFFI" "$app/libs"

"$cargo" run -p mail-ffi --bin uniffi-bindgen -- generate \
    --library "$root/target/debug/libmail_ffi.dylib" \
    --language swift \
    --out-dir "$app/Generated"

cp "$app/Generated/mail_ffiFFI.h" "$app/Sources/mail_ffiFFI/"
cp "$app/Generated/mail_ffiFFI.modulemap" "$app/Sources/mail_ffiFFI/module.modulemap"
cp "$root/target/debug/libmail_ffi.a" "$app/libs/"

swift build --package-path "$app" -c debug
