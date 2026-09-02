#!/usr/bin/env bash
#
# Build "Agent Gateway.app" and a .dmg installer.
#
# Modelled on Zed's own script/bundle-mac: assemble the bundle by hand, sign it,
# then hand a directory to hdiutil. No third-party packaging tool is involved,
# which keeps the build reproducible with nothing but Xcode and cargo.
#
# Usage:
#   script/bundle-mac.sh                 # web client + CLI + GUI, ad-hoc signed
#   script/bundle-mac.sh --no-app        # skip the GPUI app (no Xcode needed)
#   script/bundle-mac.sh --no-web        # skip the web client (no Node needed)
#   script/bundle-mac.sh --sign          # sign with MACOS_SIGNING_KEY, notarise
#                                        # when APPLE_NOTARIZATION_* are set
#   script/bundle-mac.sh --open          # reveal the .dmg in Finder afterwards
set -euo pipefail

cd "$(dirname "$0")/.."
root="$(pwd)"

build_app=true
build_web=true
sign=false
open_after=false
for argument in "$@"; do
  case "$argument" in
    --no-app) build_app=false ;;
    --no-web) build_web=false ;;
    --sign) sign=true ;;
    --open) open_after=true ;;
    -h | --help)
      sed -n '3,17p' "$0"
      exit 0
      ;;
    *)
      echo "unknown option: $argument" >&2
      exit 2
      ;;
  esac
done

arch="$(uname -m)"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
staging="target/bundle"
app="${staging}/Agent Gateway.app"
dmg="target/Agent-Gateway-${version}-${arch}.dmg"

echo "==> Agent Gateway ${version} (${arch})"

# --- 1. the web client -------------------------------------------------------
cargo_features=()
if [ "$build_web" = true ]; then
  if ! command -v pnpm > /dev/null; then
    echo "pnpm not found; install it or pass --no-web" >&2
    exit 1
  fi
  echo "==> building the web client"
  (cd web && pnpm install --frozen-lockfile && pnpm build)
  cargo_features=(--features web-ui)
else
  echo "==> skipping the web client (/app will explain how to build it)"
fi

# --- 2. the daemon and CLI ---------------------------------------------------
echo "==> building agent-gateway"
cargo build --release -p gateway-cli "${cargo_features[@]}"

# --- 3. the GUI --------------------------------------------------------------
if [ "$build_app" = true ]; then
  if ! xcrun -f metal > /dev/null 2>&1; then
    cat >&2 <<'METAL'
The Metal compiler is missing, so GPUI cannot build. Either:
  xcodebuild -downloadComponent MetalToolchain
  sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
or run this script with --no-app to package just the daemon and CLI.
METAL
    exit 1
  fi
  echo "==> building the desktop app"
  cargo build --release --manifest-path app/Cargo.toml
fi

# --- 4. assemble the bundle --------------------------------------------------
echo "==> assembling ${app}"
rm -rf "$staging"
mkdir -p "${app}/Contents/MacOS" "${app}/Contents/Resources"

cp app/resources/Info.plist "${app}/Contents/Info.plist"
printf 'APPL????' > "${app}/Contents/PkgInfo"

# The daemon ships inside the bundle so the app can start it and users get the
# CLI without a second install.
cp target/release/agent-gateway "${app}/Contents/MacOS/agent-gateway"
if [ "$build_app" = true ]; then
  cp app/target/release/agent-gateway-app "${app}/Contents/MacOS/agent-gateway-app"
else
  # Without the GUI the bundle still has to have its declared executable, so
  # point it at a stub that opens the web client instead.
  cat > "${app}/Contents/MacOS/agent-gateway-app" <<'STUB'
#!/bin/sh
here="$(cd "$(dirname "$0")" && pwd)"
"${here}/agent-gateway" run &
sleep 1
open "http://127.0.0.1:48100/app"
STUB
  chmod +x "${app}/Contents/MacOS/agent-gateway-app"
fi

# `sips -s format icns` refuses a 1024px source, so build a proper iconset and
# let iconutil assemble it — that is the supported path and gives every size
# macOS asks for.
icon_png="${staging}/AppIcon.png"
iconset="${staging}/AppIcon.iconset"
python3 script/make-icon.py "$icon_png"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$icon_png" --out "${iconset}/icon_${size}x${size}.png" > /dev/null
  sips -z "$((size * 2))" "$((size * 2))" "$icon_png" \
    --out "${iconset}/icon_${size}x${size}@2x.png" > /dev/null
done
iconutil -c icns "$iconset" -o "${app}/Contents/Resources/AppIcon.icns"
rm -rf "$iconset" "$icon_png"

# --- 5. sign ----------------------------------------------------------------
identity="-"
if [ "$sign" = true ]; then
  identity="${MACOS_SIGNING_KEY:?--sign needs MACOS_SIGNING_KEY}"
  echo "==> signing with ${identity}"
else
  echo "==> ad-hoc signing (unsigned builds need: xattr -dr com.apple.quarantine)"
fi
codesign --force --timestamp=none --options runtime \
  --sign "$identity" "${app}/Contents/MacOS/agent-gateway" > /dev/null 2>&1 ||
  codesign --force --sign "$identity" "${app}/Contents/MacOS/agent-gateway"
codesign --force --deep --options runtime \
  --sign "$identity" "$app" > /dev/null 2>&1 ||
  codesign --force --deep --sign "$identity" "$app"

# --- 6. the dmg -------------------------------------------------------------
echo "==> creating ${dmg}"
ln -s /Applications "${staging}/Applications"
rm -f "$dmg"
hdiutil create \
  -volname "Agent Gateway" \
  -srcfolder "$staging" \
  -ov -format UDZO \
  "$dmg" > /dev/null
# The symlink confuses tooling that later scans target/, and Zed hit the same
# problem; remove it once the image is written.
rm -f "${staging}/Applications"

if [ "$sign" = true ] && [ -n "${APPLE_NOTARIZATION_KEY_ID:-}" ]; then
  echo "==> notarising"
  codesign --force --sign "$identity" "$dmg"
  xcrun notarytool submit --wait \
    --key "${APPLE_NOTARIZATION_KEY:?}" \
    --key-id "${APPLE_NOTARIZATION_KEY_ID}" \
    --issuer "${APPLE_NOTARIZATION_ISSUER_ID:?}" \
    "$dmg"
  xcrun stapler staple "$dmg"
fi

size="$(du -h "$dmg" | cut -f1)"
echo
echo "built ${dmg} (${size})"
echo "  open \"${root}/${dmg}\"   # then drag the app to Applications"
[ "$open_after" = true ] && open -R "$dmg"
