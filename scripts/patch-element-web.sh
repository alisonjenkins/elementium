#!/usr/bin/env bash
# Patches Element Web's index.html to inject Elementium shims and config.
# Idempotent — skips injection if the marker comment is already present.
set -euo pipefail

cd "$(dirname "$0")/.."

DIST_DIR="element-web-dist"
SHIMS_SRC="frontend/dist-shims/elementium-shims.js"
CONFIG_SRC="element-web-config/config.json"
INDEX="$DIST_DIR/index.html"
MARKER="<!-- elementium-shims-injected -->"

if [[ ! -d "$DIST_DIR" ]]; then
    echo "[patch] ERROR: $DIST_DIR not found. Run fetch-element-web.sh first." >&2
    exit 1
fi

if [[ ! -f "$SHIMS_SRC" ]]; then
    echo "[patch] ERROR: $SHIMS_SRC not found. Run 'pnpm run build:shims' first." >&2
    exit 1
fi

if [[ ! -f "$INDEX" ]]; then
    echo "[patch] ERROR: $INDEX not found." >&2
    exit 1
fi

# 1. Copy shims bundle
cp "$SHIMS_SRC" "$DIST_DIR/elementium-shims.js"
echo "[patch] Copied shims to $DIST_DIR/elementium-shims.js"

# 2. Copy config
cp "$CONFIG_SRC" "$DIST_DIR/config.json"
echo "[patch] Copied config to $DIST_DIR/config.json"

# 3. Remove Element Web's CSP meta tag (Tauri's CSP is the security boundary)
if grep -q 'http-equiv="Content-Security-Policy"' "$INDEX"; then
    sed -i '/<meta http-equiv="Content-Security-Policy"/,/>/d' "$INDEX"
    echo "[patch] Removed Element Web CSP meta tag (Tauri CSP is active)"
fi

# 4. Inject shims script tag into index.html (before first <script> tag)
if grep -qF "$MARKER" "$INDEX"; then
    echo "[patch] Shims already injected, skipping."
else
    # Insert marker + shims script before the first <script tag only
    awk -v marker="$MARKER" '
        !done && /<script/ {
            print "    " marker
            print "    <script src=\"elementium-shims.js\"></script>"
            done = 1
        }
        { print }
    ' "$INDEX" > "$INDEX.tmp"
    mv "$INDEX.tmp" "$INDEX"
    echo "[patch] Injected shims script tag into $INDEX"
fi

# 5. Patch Element Call widget (if present) to inject IPC bridge + shims
EC_DIR="$DIST_DIR/widgets/element-call"
EC_INDEX="$EC_DIR/index.html"
EC_MARKER="<!-- elementium-ec-shims-injected -->"

if [[ -d "$EC_DIR" && -f "$EC_INDEX" ]]; then
    # Copy shims into widget directory
    cp "$SHIMS_SRC" "$EC_DIR/elementium-shims.js"
    echo "[patch] Copied shims to $EC_DIR/elementium-shims.js"

    if grep -qF "$EC_MARKER" "$EC_INDEX"; then
        echo "[patch] Element Call shims already injected, skipping."
    else
        # Inject IPC bridge + shims before the first <script> tag
        awk -v marker="$EC_MARKER" '
            !done && /<script/ {
                print "    " marker
                print "    <script>"
                print "      // Bridge Tauri IPC from parent frame into Element Call iframe"
                print "      if (window.parent && window.parent.__TAURI_INTERNALS__ && !window.__TAURI_INTERNALS__) {"
                print "        window.__TAURI_INTERNALS__ = window.parent.__TAURI_INTERNALS__;"
                print "        console.log(\"[Elementium] Bridged __TAURI_INTERNALS__ from parent into Element Call iframe\");"
                print "      }"
                print "    </script>"
                print "    <script src=\"elementium-shims.js\"></script>"
                done = 1
            }
            { print }
        ' "$EC_INDEX" > "$EC_INDEX.tmp"
        mv "$EC_INDEX.tmp" "$EC_INDEX"
        echo "[patch] Injected IPC bridge + shims into $EC_INDEX"
    fi
else
    echo "[patch] Element Call widget not found at $EC_DIR, skipping widget patch."
fi
