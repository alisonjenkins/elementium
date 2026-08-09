#!/usr/bin/env bash
# Patches Element Web's index.html to inject Elementium shims and config.
# Idempotent — skips injection if the marker comment is already present.
#
# Every step asserts its own effect. That is not defensive habit: `sed` that matches
# nothing and `awk` that finds no `<script>` both exit 0, so before this an upstream change
# to `index.html` produced a build that succeeded and an application that was broken with
# nothing anywhere reporting it. A patch step that cannot verify what it did is a patch step
# that will one day quietly do nothing.
set -euo pipefail

cd "$(dirname "$0")/.."

# Fail with the step that failed, rather than with a shell trace.
fail() {
    echo "[patch] FAILED: $*" >&2
    exit 1
}

# Assert a file exists after we claim to have produced it.
assert_file() {
    [[ -f "$1" ]] || fail "$2 (expected $1)"
}

# Assert `$2` appears in file `$1`.
assert_contains() {
    grep -qF -- "$2" "$1" || fail "$3"
}

# Assert `$2` does *not* appear in file `$1`.
assert_absent() {
    grep -qF -- "$2" "$1" && fail "$3"
    return 0
}

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

# Identifies which build of the shims bundle a frame is running, independent of the console
# bridge that reports it. Content-hashed rather than timestamped: two frames loading the
# *same* physical file always agree, and a frame that ends up with a stale copy -- the exact
# fault this whole mechanism exists to catch -- disagrees immediately, in one log line,
# instead of being inferred after the fact from a missing tag. See console-bridge.ts.
SHIM_FINGERPRINT=$(sha256sum "$SHIMS_SRC" | cut -c1-16)

# How many injections we asserted, reported at the end so a silent zero is visible.
INJECTIONS=0

# 1. Copy shims bundle
cp "$SHIMS_SRC" "$DIST_DIR/elementium-shims.js"
assert_file "$DIST_DIR/elementium-shims.js" "shims bundle was not copied"
echo "[patch] Copied shims to $DIST_DIR/elementium-shims.js"

# 2. Copy config
cp "$CONFIG_SRC" "$DIST_DIR/config.json"
assert_file "$DIST_DIR/config.json" "config was not copied"
echo "[patch] Copied config ($CONFIG_SRC) to $DIST_DIR/config.json"

# 3. Remove Element Web's CSP meta tag (Tauri's CSP is the security boundary)
#
# Asserted rather than assumed: if upstream reformats the tag onto one line, or renames the
# attribute, this `sed` matches nothing and every later script is blocked by a CSP nobody
# meant to keep.
if grep -q 'http-equiv="Content-Security-Policy"' "$INDEX"; then
    sed -i '/<meta http-equiv="Content-Security-Policy"/,/>/d' "$INDEX"
    assert_absent "$INDEX" 'http-equiv="Content-Security-Policy"' \
        "the CSP meta tag survived removal -- upstream's markup has changed shape"
    echo "[patch] Removed Element Web CSP meta tag (Tauri CSP is active)"
fi

# Set the shim fingerprint in an already-patched index, inserting the line if it is absent.
#
# Two cases, and only one of them was handled. A dist patched by a *newer* script has the
# fingerprint line and needs it refreshed, because the shims bundle is re-copied on every run
# and a fingerprint written once would drift from the file it names. A dist patched by an
# *older* script -- which is every dist on disk when this feature lands -- has no such line
# at all, so a replace-only `sed` matched nothing and the assertion that followed failed the
# whole build. Being idempotent across script versions is the requirement here, not just
# across runs.
set_shim_fingerprint() {
    local index="$1" anchor="$2"
    if grep -q "__ELEMENTIUM_SHIM_FINGERPRINT" "$index"; then
        sed -i "s/window\\.__ELEMENTIUM_SHIM_FINGERPRINT = \"[^\"]*\"/window.__ELEMENTIUM_SHIM_FINGERPRINT = \"$SHIM_FINGERPRINT\"/" "$index"
    else
        # Before the shims script, because console-bridge.ts reads the global synchronously
        # as its first act.
        awk -v fp="$SHIM_FINGERPRINT" -v anchor="$anchor" '
            !done && index($0, anchor) {
                print "    <script>window.__ELEMENTIUM_SHIM_FINGERPRINT = \"" fp "\";</script>"
                done = 1
            }
            { print }
        ' "$index" > "$index.tmp"
        mv "$index.tmp" "$index"
    fi
}

# 4. Inject shims script tag into index.html (before first <script> tag)
if grep -qF "$MARKER" "$INDEX"; then
    echo "[patch] Shims already injected, skipping."
else
    # Insert marker + fingerprint + shims script before the first <script tag only. The
    # fingerprint has to land before the shims script runs, so it is set inline rather than
    # fetched -- console-bridge.ts reads it synchronously as its very first act.
    awk -v marker="$MARKER" -v fp="$SHIM_FINGERPRINT" '
        !done && /<script/ {
            print "    " marker
            print "    <script>window.__ELEMENTIUM_SHIM_FINGERPRINT = \"" fp "\";</script>"
            print "    <script src=\"elementium-shims.js\"></script>"
            done = 1
        }
        { print }
    ' "$INDEX" > "$INDEX.tmp"
    mv "$INDEX.tmp" "$INDEX"
    # An `index.html` with no `<script>` leaves this awk a no-op, and the app then runs
    # with no shims at all: no native audio, no native camera, no key bridge.
    assert_contains "$INDEX" "$MARKER" \
        "shims were not injected into $INDEX -- it has no <script> tag to inject before"
    echo "[patch] Injected shims script tag into $INDEX"
fi
assert_contains "$INDEX" "elementium-shims.js" "shims script tag missing from $INDEX"
# Refreshed unconditionally, not only on first injection: the shims *bundle* is re-copied
# every run (step 1, above) regardless of whether the script tag was already present, so a
# fingerprint that only got written once would drift out of sync with the file it names on
# the very next `pnpm run build:shims` -- silently reintroducing the fault this fingerprint
# exists to catch.
set_shim_fingerprint "$INDEX" "elementium-shims.js"
assert_contains "$INDEX" "$SHIM_FINGERPRINT" "shim fingerprint missing from $INDEX"
INJECTIONS=$((INJECTIONS + 1))

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
    # Built here rather than assumed: the shims build empties `dist-shims`, so a bundle
    # produced before it is gone by the time this runs.
    if [[ ! -f frontend/dist-shims/elementium-autojoin.js ]]; then
        (cd frontend && pnpm exec vite build -c vite.autojoin.config.ts >/dev/null)
    fi
    # Built here rather than assumed: the shims build empties `dist-shims`, so a bundle
    # produced before it is gone by the time this runs.
    if [[ ! -f frontend/dist-shims/elementium-autojoin.js ]]; then
        (cd frontend && pnpm exec vite build -c vite.autojoin.config.ts >/dev/null)
    fi
    cp frontend/dist-shims/elementium-autojoin.js "$DIST_DIR/elementium-autojoin.js"
    # Participant index 0 by default; `just call-peers` uses the rest, so the app takes
    # tester1 and meets them.
    AUTOJOIN_JSON=$(ELEMENTIUM_AUTOJOIN_VIDEO="${ELEMENTIUM_AUTOJOIN_VIDEO:-0}" \
        ELEMENTIUM_TRACE_PC="${ELEMENTIUM_TRACE_PC:-0}" python3 - "$FIXTURE" <<'PYEOF'
import json, os, sys, urllib.request

env = json.load(open(sys.argv[1]))
hs = env.get("homeserver", "http://localhost:8008")

# A fresh login rather than the fixture's token. The fixture names one device per
# provision, and `just app-join` wipes the application's profile on every run -- so reusing
# it means a device the homeserver holds keys for against a crypto store that no longer has
# them. The key upload is then rejected ("One time key already exists") and the client never
# finishes starting, which shows up as the room never appearing.
body = json.dumps({
    "type": "m.login.password",
    "identifier": {"type": "m.id.user", "user": "tester1"},
    "password": "test-password-1",
}).encode()
req = urllib.request.Request(f"{hs}/_matrix/client/v3/login", data=body,
                             headers={"Content-Type": "application/json"})
who = json.load(urllib.request.urlopen(req))

print(json.dumps({
    "homeserver": hs,
    "userId": who["user_id"],
    "accessToken": who["access_token"],
    "deviceId": who["device_id"],
    "roomId": env["room_id"],
    "video": os.environ.get("ELEMENTIUM_AUTOJOIN_VIDEO") == "1",
    # Records every property read and method call on the peer connection, in order.
    # Off unless asked for: it is the only way to see a call the far end makes that our
    # shim does not implement, because an unimplemented call logs nothing at all.
    "tracePc": os.environ.get("ELEMENTIUM_TRACE_PC") == "1",
}))
PYEOF
)
    for f in "$INDEX" "$DIST_DIR/widgets/element-call/index.html"; do
        [[ -f "$f" ]] || continue
        # Always replaced, never skipped-if-present. The injected blob carries a room id and
        # an access token, both of which change on every provision -- so honouring an
        # existing injection points the application at the *previous* run's room, which it is
        # not in. That presents as "There's no preview, would you like to join?" for an
        # account the homeserver reports joined, and it is why this worked exactly once: on
        # the run after the marker had been cleared.
        sed -i '/__ELEMENTIUM_AUTOJOIN/d; /elementium-autojoin\.js/d' "$f"
        awk -v cfg="$AUTOJOIN_JSON" '
            !done && /<script/ {
                print "    <script>window.__ELEMENTIUM_AUTOJOIN = " cfg ";</script>"
                print "    <script src=\"/elementium-autojoin.js\"></script>"
                done = 1
            }
            { print }
        ' "$f" > "$f.tmp"
        mv "$f.tmp" "$f"
        assert_contains "$f" "__ELEMENTIUM_AUTOJOIN" \
            "autojoin config was not injected into $f"
        assert_contains "$f" "elementium-autojoin.js" \
            "autojoin driver was not injected into $f"
        echo "[patch] Injected autojoin driver into $f"
    done
    AUTOJOIN_INJECTED=true
else
    # Symmetric removal. The injection carries a real access token and joins a call on
    # startup, so leaving it behind would make an ordinary `just dev` log in as a test user
    # and dial into a call by itself. Anything that can be turned on has to be turned off by
    # the same script, or it is only off until someone forgets.
    for f in "$INDEX" "$DIST_DIR/widgets/element-call/index.html"; do
        [[ -f "$f" ]] || continue
        grep -qF "elementium-autojoin" "$f" || continue
        sed -i '/__ELEMENTIUM_AUTOJOIN/d; /elementium-autojoin\.js/d' "$f"
        # A removal that silently fails leaves a live access token and a call that dials
        # itself in an ordinary build, which is the whole reason the removal is symmetric.
        assert_absent "$f" "elementium-autojoin" "autojoin driver survived removal from $f"
        echo "[patch] Removed autojoin driver from $f"
    done
    rm -f "$DIST_DIR/elementium-autojoin.js"
    AUTOJOIN_INJECTED=false
fi

# 5. Patch Element Call widget (if present) to inject IPC bridge + shims
EC_DIR="$DIST_DIR/widgets/element-call"
EC_INDEX="$EC_DIR/index.html"
EC_MARKER="<!-- elementium-ec-shims-injected -->"

# Missing is a failure, not a thing to skip past. Element Call is how this application makes
# a call at all, so a release that stopped bundling it must not produce an app that starts
# and cannot call -- which is exactly what "skipping widget patch" used to produce.
[[ -d "$EC_DIR" ]] || fail "Element Call widget missing at $EC_DIR. Either the Element Web \
release stopped bundling it, or the fetch was incomplete. Calls cannot work without it."
assert_file "$EC_INDEX" "Element Call widget has no index.html"

# Copy shims into widget directory
cp "$SHIMS_SRC" "$EC_DIR/elementium-shims.js"
assert_file "$EC_DIR/elementium-shims.js" "shims were not copied into the widget"
echo "[patch] Copied shims to $EC_DIR/elementium-shims.js"

if grep -qF "$EC_MARKER" "$EC_INDEX"; then
    echo "[patch] Element Call shims already injected, skipping."
else
    # Inject IPC bridge + shims before the first <script> tag
    awk -v marker="$EC_MARKER" -v fp="$SHIM_FINGERPRINT" '
        !done && /<script/ {
            print "    " marker
            print "    <script>"
            print "      // Bridge Tauri IPC from parent frame into Element Call iframe"
            print "      if (window.parent && window.parent.__TAURI_INTERNALS__ && !window.__TAURI_INTERNALS__) {"
            print "        window.__TAURI_INTERNALS__ = window.parent.__TAURI_INTERNALS__;"
            print "        console.log(\"[Elementium] Bridged __TAURI_INTERNALS__ from parent into Element Call iframe\");"
            print "      }"
            print "    </script>"
            print "    <script>window.__ELEMENTIUM_SHIM_FINGERPRINT = \"" fp "\";</script>"
            print "    <script src=\"elementium-shims.js\"></script>"
            done = 1
        }
        { print }
    ' "$EC_INDEX" > "$EC_INDEX.tmp"
    mv "$EC_INDEX.tmp" "$EC_INDEX"
    assert_contains "$EC_INDEX" "$EC_MARKER" \
        "shims were not injected into the Element Call widget -- it has no <script> tag to \
inject before. The widget carries the media, so this is a silent call with no error."
    echo "[patch] Injected IPC bridge + shims into $EC_INDEX"
fi
assert_contains "$EC_INDEX" "elementium-shims.js" "shims script tag missing from $EC_INDEX"
assert_contains "$EC_INDEX" "__TAURI_INTERNALS__" "IPC bridge missing from $EC_INDEX"
# See the matching comment on the main frame's injection, above: refreshed every run so this
# frame's fingerprint cannot outlive the shims bundle it is meant to identify.
set_shim_fingerprint "$EC_INDEX" "elementium-shims.js"
assert_contains "$EC_INDEX" "$SHIM_FINGERPRINT" "shim fingerprint missing from $EC_INDEX"
INJECTIONS=$((INJECTIONS + 1))

# 6. The build record.
#
# Answers "what was actually running", which a bug report otherwise costs a round trip to
# find out. Written last, so it describes a tree every step above has already asserted.
#
# `autojoinInjected` is not diagnostics. The injection carries a live access token and dials
# a call on startup, so a release build refuses to proceed when it is set -- see
# `scripts/prepare-build.sh`. Recording it is what makes that refusal possible.
BUILD_RECORD="$DIST_DIR/.elementium-build.json"
SOURCE_INFO_FILE="$DIST_DIR/.source-info"
SOURCE_INFO=$(cat "$SOURCE_INFO_FILE" 2>/dev/null || echo "unknown")

# Element Call is not pinned separately from Element Web, so nothing else would notice it
# moving. Its asset filenames are content-hashed by its own build, which makes a hash over
# the listing a serviceable fingerprint without reading a hundred megabytes.
EC_FINGERPRINT=$(find "$EC_DIR/assets" -maxdepth 1 -type f -printf '%f\n' 2>/dev/null |
    LC_ALL=C sort | sha256sum | cut -c1-16)

python3 - "$BUILD_RECORD" "$SOURCE_INFO" "$EC_FINGERPRINT" "${AUTOJOIN_INJECTED:-false}" \
    "$CONFIG_SRC" "$INJECTIONS" "$SHIM_FINGERPRINT" <<'PYEOF'
import json, sys, datetime

record_path, source_info, ec_fingerprint, autojoin, config_src, injections, \
    shim_fingerprint = sys.argv[1:8]

# `.source-info` is written by fetch-element-web.sh as `release:<repo>:<version>` or
# `git:<repo>:<branch>:<sha>`.
parts = source_info.split(":")
kind = parts[0] if parts else "unknown"
if kind == "release":
    version, source_ref = parts[-1], None
elif kind == "git":
    version, source_ref = parts[-2], parts[-1]
else:
    kind, version, source_ref = "unknown", "unknown", None

json.dump({
    "elementWebVersion": version,
    "source": kind,
    "sourceRef": source_ref,
    "builtAt": datetime.datetime.now(datetime.timezone.utc)
                   .strftime("%Y-%m-%dT%H:%M:%SZ"),
    # Empty until a patch is carried. Present rather than omitted: an absent field and an
    # empty list mean different things.
    "patches": [],
    "elementCallFingerprint": ec_fingerprint,
    "autojoinInjected": autojoin == "true",
    "configSource": config_src,
    "injectionsAsserted": int(injections),
    # What every frame's own console-bridge line should say (see console-bridge.ts). Two
    # frames disagreeing with each other, or with this, is a stale copy -- previously
    # invisible, because both frames read from the same source file name in the same repo and
    # "looked" identical until you diffed their bytes.
    "shimFingerprint": shim_fingerprint,
}, open(record_path, "w"), indent=2)
PYEOF

assert_file "$BUILD_RECORD" "build record was not written"
echo "[patch] Wrote build record to $BUILD_RECORD ($INJECTIONS injections asserted)"
