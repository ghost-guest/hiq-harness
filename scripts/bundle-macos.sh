#!/bin/bash
# 打包 hiq-harness 桌面版为 macOS .app
#
# 方式参考 orbit 的 scripts/：GPUI 应用必须是 app bundle 才有
# 窗口焦点与通知能力（裸 binary 在 macOS 上行为不完整）。
#
# 用法：./scripts/bundle-macos.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="hiq-harness"
APP="$ROOT/dist/$APP_NAME.app"
BIN="$ROOT/target/release/hiq-harness"

if [ ! -f "$BIN" ]; then
  echo "找不到 $BIN，先运行：cargo build --release -p harness-desktop"
  exit 1
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/$APP_NAME"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>hiq-harness</string>
  <key>CFBundleDisplayName</key><string>hiq-harness</string>
  <key>CFBundleIdentifier</key><string>dev.hiq.harness</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>hiq-harness</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
  <!-- 不显示 Dock 图标时要设为 1；当前需要正常窗口，保持默认 -->
  <key>LSUIElement</key><false/>
</dict>
</plist>
PLIST

# 自签名（本地运行够用；分发需 Developer ID）
codesign --force --deep --sign - "$APP" 2>/dev/null || echo "签名跳过（本地运行不受影响）"

echo "已打包：$APP"
echo "大小：$(du -sh "$APP" | cut -f1)"
echo
echo "打开：open \"$APP\""
