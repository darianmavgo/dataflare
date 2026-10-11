#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

APP_NAME="Sqliteflare"
BUNDLE="target/debug/bundle/macos/$APP_NAME.app"
DEST="/Applications/$APP_NAME.app"

if command -v pnpm >/dev/null 2>&1; then
    pnpm build:debug
else
    npx tsc
    npx vite build
    npx tauri build --debug --bundles app --config '{"build":{"beforeBuildCommand":""}}'
fi

osascript -e "quit app \"$APP_NAME\"" 2>/dev/null || true
sleep 1

rm -rf "$DEST"
cp -R "$BUNDLE" "$DEST"
xattr -dr com.apple.quarantine "$DEST" 2>/dev/null || true

echo "Installed $DEST"
open "$DEST"
