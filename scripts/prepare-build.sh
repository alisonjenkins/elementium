#!/usr/bin/env bash
# Build preparation: fetch Element Web, compile shims, patch.
# Called by Tauri's beforeBuildCommand.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "=== Elementium: preparing build ==="

./scripts/fetch-element-web.sh
cd frontend && pnpm run build:shims && cd ..
./scripts/patch-element-web.sh

# The autojoin driver logs in as a test user and dials a call on startup, from an access
# token baked into the page. It has no business in anything shipped, and "the patch script
# removes it when not asked for" is only true until that removal one day fails quietly.
# This is the second lock, on the one path where the consequence is a release.
if python3 -c 'import json,sys; sys.exit(0 if json.load(open("element-web-dist/.elementium-build.json"))["autojoinInjected"] else 1)' 2>/dev/null; then
    echo "=== Elementium: REFUSING TO BUILD ===" >&2
    echo "The autojoin driver is injected into element-web-dist. It carries a live access" >&2
    echo "token and joins a call on startup, so it must never reach a release build." >&2
    echo "Re-run without ELEMENTIUM_AUTOJOIN=1." >&2
    exit 1
fi

echo "=== Elementium: build preparation complete ==="
