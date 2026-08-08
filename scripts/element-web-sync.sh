#!/usr/bin/env bash
# Move to an Element Web release and find out whether we still work on it.
#
# One command and a report, rather than four commands and a build log to search. The
# contract is in `specs/007-element-web-upgrade/contracts/cli.md`.
#
# The pin is only written on success. A half-applied upgrade is worse than none: the next
# person cannot tell which version they are debugging, and `element-web-dist/` is rebuilt
# from scratch by the fetch, so there is nothing left to compare against.
set -euo pipefail

cd "$(dirname "$0")/.."

TARGET="${1:-}"
if [[ -z "$TARGET" ]]; then
    echo "usage: just element-web-sync <version>   (e.g. v1.12.25)" >&2
    exit 2
fi

# shellcheck source=../elementium.config.sh
source ./elementium.config.sh
CURRENT="$ELEMENT_WEB_VERSION"

if [[ "$CURRENT" == "$TARGET" ]]; then
    echo "element-web is already pinned to $TARGET"
fi

echo "element-web sync $CURRENT -> $TARGET"

step() { printf '  %-15s' "$1"; }
ok() { echo "ok${1:+    $1}"; }

# Everything below runs against the target version without touching the pin, so a failure
# leaves the configuration describing the version that actually works.
export ELEMENT_WEB_VERSION="$TARGET"

fail_and_restore() {
    echo
    echo "FAILED: $1" >&2
    echo "The pin is unchanged at $CURRENT. Restoring it." >&2
    ELEMENT_WEB_VERSION="$CURRENT" ./scripts/fetch-element-web.sh >/dev/null 2>&1 || true
    (cd frontend && pnpm run build:shims >/dev/null 2>&1) || true
    ./scripts/patch-element-web.sh >/dev/null 2>&1 || true
    exit 1
}

step fetch
./scripts/fetch-element-web.sh >/dev/null 2>&1 || fail_and_restore "could not fetch $TARGET"
ok

step shims
(cd frontend && pnpm run build:shims >/dev/null 2>&1) || fail_and_restore "shims did not build"
ok

step patch
PATCH_OUT=$(./scripts/patch-element-web.sh 2>&1) || {
    echo "FAILED"
    echo "$PATCH_OUT" | grep FAILED >&2 || echo "$PATCH_OUT" >&2
    fail_and_restore "a patch step could not verify its own effect"
}
ok "$(grep -c 'Injected\|already injected' <<<"$PATCH_OUT") injections asserted"

# The gate. Everything above proves the files were changed; this proves the changes took
# effect in a running browser, which is the only thing that answers "do the shims install
# on this version".
step "shim contract"
if (cd frontend && pnpm exec playwright test tests/matrixrtc/shim-contract.spec.ts \
    --reporter=line >/tmp/elementium-shim-contract.log 2>&1); then
    ok
else
    echo "FAIL"
    grep -E "Error:|✘" /tmp/elementium-shim-contract.log | head -10 >&2
    fail_and_restore "the shims do not install on $TARGET"
fi

# Printed whether or not it passed: it is the context for reading everything above.
echo
echo "  release notes  ${ELEMENT_WEB_REPO}/releases"
echo "                 between $CURRENT and $TARGET"

# Success: write the pin, so the configuration and the tree agree.
sed -i "s|: \"\${ELEMENT_WEB_VERSION:=.*}\"|: \"\${ELEMENT_WEB_VERSION:=$TARGET}\"|" \
    elementium.config.sh
echo
echo "element-web is now pinned to $TARGET."
echo "The shims install. That is not the same as calls working -- run the media check:"
echo "  just call-peers        # in one terminal"
echo "  just app-join          # in another"
echo "See specs/007-element-web-upgrade/quickstart.md for what the numbers should be."
