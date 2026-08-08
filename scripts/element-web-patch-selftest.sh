#!/usr/bin/env bash
# Exercise the whole carry-and-contribute cycle against a synthetic upstream.
#
# A patch workflow nobody has run is a patch workflow that does not work. This runs it: a
# change is carried, offered as a pull-request branch, taken by a stand-in upstream, and
# then *disappears* on the next rebase with nobody editing anything. That last step is the
# claim the whole arrangement rests on, and it is the one that cannot be verified by reading.
#
# Synthetic on purpose. The real fork is someone's account, and a self-test that pushed to
# it would be a self-test you could only run once.
set -euo pipefail

cd "$(dirname "$0")/.."
REPO="$PWD"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

export GIT_AUTHOR_NAME=selftest GIT_AUTHOR_EMAIL=selftest@invalid
export GIT_COMMITTER_NAME=selftest GIT_COMMITTER_EMAIL=selftest@invalid

PASS=0
FAIL=0
check() {
    local what="$1" expected="$2" actual="$3"
    if [[ "$actual" == *"$expected"* ]]; then
        printf '  ok    %s\n' "$what"
        PASS=$((PASS + 1))
    else
        printf '  FAIL  %s\n' "$what"
        printf '        expected to find: %s\n' "$expected"
        printf '        in: %s\n' "$actual"
        FAIL=$((FAIL + 1))
    fi
}

git_q() { git -c commit.gpgsign=false -c tag.gpgsign=false "$@"; }

# --- a stand-in upstream, and a fork of it ------------------------------------------------
mkdir -p "$WORK/upstream"
git_q init -q --initial-branch=develop "$WORK/upstream"
cd "$WORK/upstream"
echo base >file.txt
echo other >other.txt
git_q add -A
git_q commit -qm "upstream base"
git_q tag -a -m v1 v1

git_q clone -q "$WORK/upstream" "$WORK/fork"
cd "$WORK/fork"
git_q checkout -q -b elementium v1

printf 'base\ntaken\n' >file.txt
git_q commit -qam "a change upstream will take

Elementium-Why: upstream wants this too
Elementium-Intent: upstream"

echo ours >ours.txt
git_q add -A
git_q commit -qm "a change that stays ours

Elementium-Why: only makes sense with a native backend
Elementium-Intent: permanent-fork"

OFFERED=$(git_q rev-parse HEAD~1)

# --- the harness the scripts read ---------------------------------------------------------
mkdir -p "$WORK/run/scripts"
cp "$REPO/scripts/element-web-rebase.sh" "$REPO/scripts/element-web-pr.sh" \
    "$REPO/scripts/element-web-patches.sh" "$WORK/run/scripts/"
write_config() {
    cat >"$WORK/run/elementium.config.sh" <<EOF
: "\${ELEMENT_WEB_FORK:=$WORK/fork}"
: "\${ELEMENT_WEB_UPSTREAM:=$WORK/upstream}"
: "\${ELEMENT_WEB_PATCH_BRANCH:=elementium}"
: "\${ELEMENT_WEB_PATCH_BASE:=$1}"
EOF
}
write_config v1
cd "$WORK/run"

echo "element-web patch workflow self-test"
echo

# --- 1. the manifest lists what is carried ------------------------------------------------
git_q clone -q "$WORK/fork" "$WORK/run/.element-web-src"
git_q -C "$WORK/run/.element-web-src" checkout -q elementium
OUT=$(./scripts/element-web-patches.sh 2>&1 || true)
MANIFEST=$(cat "$WORK/run/element-web-patches.md")
check "the manifest lists both carried patches" "2 patch(es)" "$OUT"
check "it reports why a patch is carried" "upstream wants this too" "$MANIFEST"
check "it reports a patch we do not mean to offer" "permanent-fork" "$MANIFEST"

# --- 2. a carried commit becomes a pull-request branch ------------------------------------
OUT=$(./scripts/element-web-pr.sh "$OFFERED" 2>&1 || true)
check "a change meant for upstream becomes a PR branch" "pr/a-change-upstream-will-take" "$OUT"
check "it does not push" "Nothing has been pushed" "$OUT"
check "it names the CLA, which no script can sign" "cla-assistant.io" "$OUT"

OUT=$(./scripts/element-web-pr.sh elementium 2>&1 || true)
check "a permanent-fork change is refused" "not meant to go upstream" "$OUT"

# --- 3. upstream takes it ------------------------------------------------------------------
# Committed onto upstream rather than merged from the branch, so the content matches and the
# commit does not: the same situation as a maintainer applying a patch from a pull request.
cd "$WORK/upstream"
printf 'base\ntaken\n' >file.txt
git_q commit -qam "a change upstream will take"
git_q tag -a -m v2 v2

# --- 4. and the rebase notices, with nobody editing anything -------------------------------
cd "$WORK/run"
git_q -C .element-web-src checkout -q elementium
OUT=$(./scripts/element-web-rebase.sh v2 2>&1 || true)
check "the rebase reports the taken change as dropped" "dropped" "$OUT"
check "it says what dropped means" "upstream has this" "$OUT"
check "the change that stays ours still applies" "applied" "$OUT"
check "one patch is left" "1 patch(es) now carried against v2" "$OUT"

# --- 5. and the manifest agrees ------------------------------------------------------------
write_config v2
OUT=$(./scripts/element-web-patches.sh 2>&1 || true)
MANIFEST=$(cat "$WORK/run/element-web-patches.md")
check "the manifest now lists one patch" "1 patch(es)" "$OUT"
check "the contributed change is gone from it" "only makes sense with a native backend" "$MANIFEST"
if grep -q "upstream wants this too" <<<"$MANIFEST"; then
    printf '  FAIL  the contributed change should have left the manifest\n'
    FAIL=$((FAIL + 1))
else
    printf '  ok    the contributed change left the manifest\n'
    PASS=$((PASS + 1))
fi

echo
echo "  $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]] || exit 1
