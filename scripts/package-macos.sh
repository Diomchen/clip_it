#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
DIST_DIR="${DIST_DIR:-$ROOT_DIR/dist}"
if [[ -z "${VERSION:-}" ]]; then
    VERSION="$(awk -F ' *= *' '/^version *=/ { gsub(/"/, "", $2); print $2; exit }' "$ROOT_DIR/Cargo.toml")"
fi
HOST_TARGET="$(rustc -vV | awk '/^host:/ { print $2 }')"
MACOS_TARGETS="${MACOS_TARGETS:-$HOST_TARGET}"
SIGN_IDENTITY="${MACOS_SIGN_IDENTITY:--}"

if [[ "$HOST_TARGET" != *-apple-darwin ]]; then
    echo "package-macos.sh 必须在 macOS 上运行" >&2
    exit 1
fi

mkdir -p "$DIST_DIR"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/clip-it-dmg.XXXXXX")"
trap 'rm -rf "$WORK_DIR"' EXIT

BINARIES=()
for target in $MACOS_TARGETS; do
    case "$target" in
        aarch64-apple-darwin|x86_64-apple-darwin) ;;
        *)
            echo "不支持的 macOS target: $target" >&2
            exit 1
            ;;
    esac
    cargo build --manifest-path "$ROOT_DIR/Cargo.toml" --release --locked --target "$target"
    BINARIES+=("$ROOT_DIR/target/$target/release/clip-it")
done

APP_DIR="$WORK_DIR/dmg/ClipIt.app"
CONTENTS_DIR="$APP_DIR/Contents"
mkdir -p "$CONTENTS_DIR/MacOS" "$CONTENTS_DIR/Resources"
cp "$ROOT_DIR/assets/app-icon.icns" "$CONTENTS_DIR/Resources/AppIcon.icns"

if [[ "${#BINARIES[@]}" -eq 1 ]]; then
    cp "${BINARIES[0]}" "$CONTENTS_DIR/MacOS/clip-it"
    case "$MACOS_TARGETS" in
        aarch64-apple-darwin) PACKAGE_ARCH="arm64" ;;
        x86_64-apple-darwin) PACKAGE_ARCH="x86_64" ;;
    esac
else
    lipo -create "${BINARIES[@]}" -output "$CONTENTS_DIR/MacOS/clip-it"
    PACKAGE_ARCH="universal"
fi
chmod 755 "$CONTENTS_DIR/MacOS/clip-it"

cat > "$CONTENTS_DIR/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>zh_CN</string>
  <key>CFBundleDisplayName</key><string>ClipIt</string>
  <key>CFBundleExecutable</key><string>clip-it</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleIdentifier</key><string>dev.clip-it.app</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>ClipIt</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

plutil -lint "$CONTENTS_DIR/Info.plist"
codesign --force --deep --options runtime --sign "$SIGN_IDENTITY" "$APP_DIR"
codesign --verify --deep --strict --verbose=2 "$APP_DIR"

ZIP_PATH="$DIST_DIR/ClipIt-${VERSION}-macos-${PACKAGE_ARCH}.zip"
rm -f "$ZIP_PATH"
ditto -c -k --sequesterRsrc --keepParent "$APP_DIR" "$ZIP_PATH"
shasum -a 256 "$ZIP_PATH" > "$ZIP_PATH.sha256"

ln -s /Applications "$WORK_DIR/dmg/Applications"
cat > "$WORK_DIR/dmg/安装说明.txt" <<'EOF'
ClipIt 安装说明

1. 把 ClipIt.app 拖入 Applications（应用程序）文件夹。
2. 双击 ClipIt.app。应用会自动安装 Finder 右键快速操作、启用登录启动，
   并在菜单栏运行，无需执行终端命令。
3. 在 Finder 中右键文件，选择“快速操作 → 使用 ClipIt 发送”。

卸载 Finder 快速操作：

   /Applications/ClipIt.app/Contents/MacOS/clip-it integrate remove

未使用 Apple Developer ID 签名的构建首次启动时，请在 Finder 中右键
ClipIt.app 并选择“打开”，然后确认打开。
EOF

DMG_PATH="$DIST_DIR/ClipIt-${VERSION}-macos-${PACKAGE_ARCH}.dmg"
rm -f "$DMG_PATH"
hdiutil create \
    -volname "ClipIt $VERSION" \
    -srcfolder "$WORK_DIR/dmg" \
    -format UDZO \
    -imagekey zlib-level=9 \
    -ov \
    "$DMG_PATH"

shasum -a 256 "$DMG_PATH" > "$DMG_PATH.sha256"
echo "已生成 $DMG_PATH 和 $ZIP_PATH"
