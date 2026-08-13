#!/usr/bin/env bash
# Assemble a double-clickable macOS app bundle from the release binary:
#   dist/harness-terminal.app
# Run from the repo root. Rebuild the release binary first with --rebuild (or if it's missing);
# otherwise the last `cargo build --release` binary is reused. Idempotent: re-running refreshes
# the bundle in place (Finder shows the new build — no tmpfs container needed).
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="target/release/harness-terminal"
APP="dist/harness-terminal.app"

if [ "${1:-}" = "--rebuild" ] || [ ! -x "$BIN" ]; then
  cargo build --release
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# ── Info.plist ─────────────────────────────────────────────────────────────
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>harness-terminal</string>
  <key>CFBundleIdentifier</key>
  <string>ai.autonomous.harness-terminal</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>harness-terminal</string>
  <key>CFBundleDisplayName</key>
  <string>harness-terminal</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.1.0</string>
  <key>CFBundleVersion</key>
  <string>1</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
</dict>
</plist>
PLIST

# ── App icon (1024px source -> iconset -> .icns) ───────────────────────────
ICON_BUILD="$(mktemp -d)"
python3 - "$ICON_BUILD/icon_1024.png" <<'PY'
import sys
from PIL import Image, ImageDraw, ImageFont

S = 1024
im = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(im)

# Full-bleed dark backdrop (macOS rounds the corners itself).
d.rounded_rectangle([0, 0, S, S], radius=230, fill=(16, 20, 26, 255))

# Terminal window panel.
panel = [150, 210, 874, 870]
d.rounded_rectangle(panel, radius=64, fill=(7, 11, 16, 255),
                    outline=(92, 108, 128, 255), width=16)

# Titlebar dots.
for i, col in enumerate([(255, 95, 86), (255, 189, 46), (43, 202, 115)]):
    cx = 220 + i * 88
    d.ellipse([cx - 26, 278 - 26, cx + 26, 278 + 26], fill=col + (255,))

# Prompt glyph: "> " in a bright green, then a block cursor.
try:
    font = ImageFont.truetype("/System/Library/Fonts/Menlo.ttc", 300)
except Exception:
    font = ImageFont.load_default()
d.text((248, 470), ">", font=font, fill=(55, 232, 128, 255))
# Cursor block after the prompt.
d.rounded_rectangle([600, 470, 620, 770], radius=24, fill=(55, 232, 128, 255))
im.save(sys.argv[1])
PY

ICONSET="$ICON_BUILD/AppIcon.iconset"
mkdir -p "$ICONSET"
for spec in "16x16 16" "16x16@2x 32" "32x32 32" "32x32@2x 64" \
            "128x128 128" "128x128@2x 256" "256x256 256" "256x256@2x 512" \
            "512x512 512" "512x512@2x 1024"; do
  name="${spec%% *}"; px="${spec##* }"
  sips -z "$px" "$px" "$ICON_BUILD/icon_1024.png" --out "$ICONSET/icon_$name.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$ICON_BUILD"

# ── Binary + signing ────────────────────────────────────────────────────────
cp "$BIN" "$APP/Contents/MacOS/harness-terminal"
chmod +x "$APP/Contents/MacOS/harness-terminal"
codesign --force --sign - "$APP" >/dev/null 2>&1

plutil -lint "$APP/Contents/Info.plist" >/dev/null
codesign --verify --deep --strict "$APP" >/dev/null 2>&1 && echo "codesign: OK (ad-hoc)"
echo "bundle: $APP"
