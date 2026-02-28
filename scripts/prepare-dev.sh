#!/usr/bin/env bash
# Dev preparation: fetch Element Web, compile shims, patch, then serve.
# Called by Tauri's beforeDevCommand.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "=== Elementium: preparing dev ==="

./scripts/fetch-element-web.sh
cd frontend && pnpm run build:shims && cd ..
./scripts/patch-element-web.sh

echo "=== Elementium: serving element-web-dist on port 5173 ==="
exec npx http-server element-web-dist -p 5173 -c-1
