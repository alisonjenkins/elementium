#!/usr/bin/env bash
# Generate `element-web-patches.md` from the patch branch.
#
# Generated, never hand-written. A hand-written list of patches is true on the day it is
# written and slowly stops being true: a commit gets rebased away because upstream took it,
# and the list still claims we carry it. Reading the branch cannot drift from the branch.
#
# Each carried commit says three things about itself, in trailers, because the commit is the
# only place that survives a rebase:
#
#   Elementium-Intent: upstream | permanent-fork
#       Whether we mean to offer this. `permanent-fork` is what `element-web-pr` refuses.
#   Elementium-Why: <one line>
#       Why it exists. The subject says what it does; this says why we carry it.
#   Elementium-Upstream: <pull request url>
#       Present once it has been offered. How "offered" is told from "not offered yet".
set -euo pipefail

cd "$(dirname "$0")/.."

# shellcheck source=../elementium.config.sh
source ./elementium.config.sh

SRC=".element-web-src"
OUT="element-web-patches.md"

fail() {
    echo "FAILED: $1" >&2
    exit 1
}

[[ -d "$SRC/.git" ]] || fail "no Element Web checkout at $SRC. Run 'just element-web-rebase <version>' first."

BRANCH="$ELEMENT_WEB_PATCH_BRANCH"
BASE="${ELEMENT_WEB_PATCH_BASE:-}"
[[ -n "$BASE" ]] || fail "ELEMENT_WEB_PATCH_BASE is unset in elementium.config.sh; there is no base to list patches against"

trailer() {
    git -C "$SRC" log -1 --format=%B "$1" | sed -n "s/^$2: *//p" | head -1
}

if git -C "$SRC" rev-parse --verify --quiet "refs/heads/$BRANCH" >/dev/null; then
    mapfile -t CARRIED < <(git -C "$SRC" rev-list --reverse "$BASE..$BRANCH")
else
    CARRIED=()
fi

{
    echo "# Patches carried against Element Web"
    echo
    echo "**Generated** by \`just element-web-patches\` — do not edit by hand."
    echo
    echo "Every row is a commit on \`$BRANCH\` in [the fork]($ELEMENT_WEB_FORK), rebased onto"
    echo "\`$BASE\`. A change upstream accepts disappears from this list at the next rebase,"
    echo "because rebase drops a commit whose patch-id upstream already has — so an empty list"
    echo "means we carry nothing, not that nobody has updated the file."
    echo

    if [[ ${#CARRIED[@]} -eq 0 ]]; then
        echo "**Nothing is currently carried.** Element Web is used unmodified at \`$BASE\`;"
        echo "everything Elementium changes is a runtime shim or build-time injection, which"
        echo "\`docs/element-web.md\` explains the reasoning for."
    else
        echo "| Commit | Change | Why | Intent | Offered |"
        echo "|---|---|---|---|---|"
        for sha in "${CARRIED[@]}"; do
            short=$(git -C "$SRC" rev-parse --short "$sha")
            subject=$(git -C "$SRC" log -1 --format=%s "$sha")
            why=$(trailer "$sha" "Elementium-Why")
            intent=$(trailer "$sha" "Elementium-Intent")
            pr=$(trailer "$sha" "Elementium-Upstream")
            # An absent trailer is reported as absent rather than blank. "No stated reason"
            # is a finding about the commit; an empty cell reads as a formatting slip.
            # shellcheck disable=SC2016  # single quotes are deliberate: this is a
            # printf format string, and the backticks are markdown, not substitution.
            printf '| `%s` | %s | %s | %s | %s |\n' \
                "$short" "$subject" \
                "${why:-_not stated_}" \
                "${intent:-_not stated_}" \
                "${pr:-not yet}"
        done
    fi
} >"$OUT"

echo "wrote $OUT (${#CARRIED[@]} patch(es) against $BASE)"
