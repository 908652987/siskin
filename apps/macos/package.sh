#!/usr/bin/env bash
# Assembles Siskin.app from a release Rust FFI build and the SwiftPM products.
# The bundle is ad-hoc signed: enough to launch locally, not for distribution.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
app="$root/apps/macos"
cargo="${CARGO:-$HOME/.cargo/bin/cargo}"
bundle="$app/dist/Siskin.app"

"$cargo" build -p mail-ffi --release
mkdir -p "$app/Generated" "$app/Sources/mail_ffiFFI" "$app/libs"

"$cargo" run -p mail-ffi --release --bin uniffi-bindgen -- generate \
    --library "$root/target/release/libmail_ffi.dylib" \
    --language swift \
    --out-dir "$app/Generated"

cp "$app/Generated/mail_ffiFFI.h" "$app/Sources/mail_ffiFFI/"
cp "$app/Generated/mail_ffiFFI.modulemap" "$app/Sources/mail_ffiFFI/module.modulemap"
cp "$root/target/release/libmail_ffi.a" "$app/libs/"

swift build --package-path "$app" -c release

rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "$app/.build/release/Siskin" "$bundle/Contents/MacOS/Siskin"
cp -R "$app/.build/release/Siskin_SiskinApp.bundle" "$bundle/Contents/Resources/"
cp "$app/Info.plist" "$bundle/Contents/Info.plist"

codesign --force --sign - --timestamp=none "$bundle"

echo "built $bundle"
