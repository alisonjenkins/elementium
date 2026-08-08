#!/usr/bin/env bash
# Turn a carried commit into a branch that can be opened as a pull request.
#
# The contract is in `specs/007-element-web-upgrade/contracts/cli.md`.
#
# The point of the whole arrangement is that this needs no translation step: a change we
# carry is already an ordinary commit against an upstream tag, so offering it is a
# cherry-pick and nothing else. If this script had to *convert* anything, the patches would
# be in the wrong form.
set -euo pipefail

cd "$(dirname "$0")/.."

COMMIT="${1:-}"
if [[ -z "$COMMIT" ]]; then
    echo "usage: just element-web-pr <commit>   (a commit on the patch branch)" >&2
    exit 2
fi

# shellcheck source=../elementium.config.sh
source ./elementium.config.sh

SRC=".element-web-src"

fail() {
    echo "FAILED: $1" >&2
    exit 1
}

[[ -d "$SRC/.git" ]] || fail "no Element Web checkout at $SRC. Run 'just element-web-rebase <version>' first."
# An `if` rather than `[[ ... ]] && fail`: the `&&` form makes the whole statement's exit
# status 1 whenever the test is false, which under `set -e` is a script that stops silently
# on the *success* path.
if [[ -n "$(git -C "$SRC" status --porcelain)" ]]; then
    fail "the checkout at $SRC has uncommitted changes; commit or clean them first"
fi

git -C "$SRC" rev-parse --verify --quiet "$COMMIT^{commit}" >/dev/null ||
    fail "$COMMIT is not a commit in $SRC"

SHA=$(git -C "$SRC" rev-parse --short "$COMMIT")
SUBJECT=$(git -C "$SRC" log -1 --format=%s "$COMMIT")
BODY=$(git -C "$SRC" log -1 --format=%B "$COMMIT")

# A commit classified as one we do not intend to offer must not be offered by accident. If
# that classification has changed, the classification is what should change -- in the
# commit -- rather than being overridden here where nothing records the decision.
if grep -q '^Elementium-Intent: *permanent-fork' <<<"$BODY"; then
    echo "$SHA is marked 'Elementium-Intent: permanent-fork':" >&2
    echo "  $SUBJECT" >&2
    fail "that commit is not meant to go upstream. Change the trailer first if it is now."
fi

BASE="${ELEMENT_WEB_PATCH_BASE:-}"
[[ -n "$BASE" ]] || fail "ELEMENT_WEB_PATCH_BASE is unset in elementium.config.sh; there is no upstream tag to branch from"
git -C "$SRC" rev-parse --verify --quiet "refs/tags/$BASE" >/dev/null ||
    fail "$SRC has no tag $BASE. Run 'just element-web-rebase $BASE' first."

# One commit per branch, named after it. A PR branch carrying two unrelated changes is two
# reviews in one thread, which is the thing that makes contributing back expensive.
SLUG=$(tr '[:upper:]' '[:lower:]' <<<"$SUBJECT" | sed -E 's/[^a-z0-9]+/-/g; s/^-+|-+$//g' | cut -c1-48)
# `pr/<slug>` rather than `<patch-branch>/<slug>`: git refs are paths, so a branch named
# `elementium` and a branch named `elementium/anything` cannot both exist. Naming these
# after the patch branch made the first one fail with "cannot lock ref ... 'refs/heads/
# elementium' exists", which reads like repository corruption rather than a naming clash.
PR_BRANCH="pr/$SLUG"

git -C "$SRC" rev-parse --verify --quiet "refs/heads/$PR_BRANCH" >/dev/null &&
    fail "branch $PR_BRANCH already exists in $SRC. Delete it or rename this one."

git -C "$SRC" checkout --quiet -b "$PR_BRANCH" "$BASE" || fail "could not branch from $BASE"
if ! git -C "$SRC" cherry-pick "$COMMIT" >/dev/null 2>&1; then
    git -C "$SRC" cherry-pick --abort 2>/dev/null || true
    git -C "$SRC" checkout --quiet - 2>/dev/null || true
    git -C "$SRC" branch -D "$PR_BRANCH" >/dev/null 2>&1 || true
    fail "$SHA does not apply cleanly to $BASE. Rebase the patch branch onto $BASE first."
fi

cat <<REPORT

  $PR_BRANCH
    from   $BASE
    commit $SHA  $SUBJECT

Nothing has been pushed. Element Web requires a CLA rather than a DCO, signed once per
person at https://cla-assistant.io/element-hq/element-web, before a pull request can be
merged. When you are ready:

  git -C $SRC push origin $PR_BRANCH
  gh pr create --repo ${ELEMENT_WEB_UPSTREAM##*/github.com/} --head <your-user>:$PR_BRANCH

Then record the pull request on the carried commit, so the next rebase can tell a change
that was offered from one that was not:

  Elementium-Upstream: <pull request url>
REPORT
