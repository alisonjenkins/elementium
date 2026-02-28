#!/usr/bin/env bash
# Build preparation: fetch Element Web, compile shims, patch.
# Called by Tauri's beforeBuildCommand.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "=== Elementium: preparing build ==="

./scripts/fetch-element-web.sh
cd frontend && pnpm run build:shims && cd ..
./scripts/patch-element-web.sh

echo "=== Elementium: build preparation complete ==="
