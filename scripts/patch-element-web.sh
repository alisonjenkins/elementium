#!/usr/bin/env bash
# Patches Element Web's index.html to inject Elementium shims and config.
# Idempotent — skips injection if the marker comment is already present.
set -euo pipefail

cd "$(dirname "$0")/.."

DIST_DIR="element-web-dist"
SHIMS_SRC="frontend/dist-shims/elementium-shims.js"
# Which homeserver the app talks to. `ELEMENTIUM_TEST_ENV=1` selects the local
# MatrixRTC stack in `test-env/`, so the app and the Playwright participants meet in
# the same room -- without it they would be on different homeservers and never see
# each other, which looks exactly like a broken call.
if [[ "${ELEMENTIUM_TEST_ENV:-}" == "1" ]]; then
    CONFIG_SRC="element-web-config/config.test-env.json"
else
    CONFIG_SRC="element-web-config/config.json"
fi
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
echo "[patch] Copied config ($CONFIG_SRC) to $DIST_DIR/config.json"

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

# 4b. Autojoin driver, for testing only.
#
# Every remaining question about the call faults needs Elementium itself in a call, and
# Playwright cannot drive it -- it is a Tauri app behind a WebKit webview. So the app drives
# itself, from credentials in the test-env fixture. Injected only when asked for, and built
# from its own entry point, so it cannot reach a release build by accident.
if [[ "${ELEMENTIUM_AUTOJOIN:-}" == "1" ]]; then
    FIXTURE="target/test-env-fixture.json"
    if [[ ! -f "$FIXTURE" ]]; then
        echo "[patch] ERROR: ELEMENTIUM_AUTOJOIN=1 but $FIXTURE is missing." >&2
        echo "[patch]        Run test-env/provision.sh first." >&2
        exit 1
    fi
    cp frontend/dist-shims/elementium-autojoin.js "$DIST_DIR/elementium-autojoin.js"
    # Participant index 0 by default; `just call-peers` uses the rest, so the app takes
    # tester1 and meets them.
    AUTOJOIN_JSON=$(ELEMENTIUM_AUTOJOIN_VIDEO="${ELEMENTIUM_AUTOJOIN_VIDEO:-0}" python3 - "$FIXTURE" <<'PYEOF'
import json, os, sys
env = json.load(open(sys.argv[1]))
who = env["participants"][0]
print(json.dumps({
    "homeserver": env.get("homeserver", "http://localhost:8008"),
    "userId": who["user_id"],
    "accessToken": who["access_token"],
    "deviceId": who["device_id"],
    "roomId": env["room_id"],
    "video": os.environ.get("ELEMENTIUM_AUTOJOIN_VIDEO") == "1",
}))
PYEOF
)
    for f in "$INDEX" "$DIST_DIR/widgets/element-call/index.html"; do
        [[ -f "$f" ]] || continue
        grep -qF "elementium-autojoin.js" "$f" && continue
        awk -v cfg="$AUTOJOIN_JSON" '
            !done && /<script/ {
                print "    <script>window.__ELEMENTIUM_AUTOJOIN = " cfg ";</script>"
                print "    <script src=\"/elementium-autojoin.js\"></script>"
                done = 1
            }
            { print }
        ' "$f" > "$f.tmp"
        mv "$f.tmp" "$f"
        echo "[patch] Injected autojoin driver into $f"
    done
else
    # Symmetric removal. The injection carries a real access token and joins a call on
    # startup, so leaving it behind would make an ordinary `just dev` log in as a test user
    # and dial into a call by itself. Anything that can be turned on has to be turned off by
    # the same script, or it is only off until someone forgets.
    for f in "$INDEX" "$DIST_DIR/widgets/element-call/index.html"; do
        [[ -f "$f" ]] || continue
        grep -qF "elementium-autojoin" "$f" || continue
        sed -i '/__ELEMENTIUM_AUTOJOIN/d; /elementium-autojoin\.js/d' "$f"
        echo "[patch] Removed autojoin driver from $f"
    done
    rm -f "$DIST_DIR/elementium-autojoin.js"
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
