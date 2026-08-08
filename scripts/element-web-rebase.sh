#!/usr/bin/env bash
# Move the patch branch onto a new upstream tag, and say what happened to each commit.
#
# The contract is in `specs/007-element-web-upgrade/contracts/cli.md`.
#
# The whole point of this arrangement is that carrying a change and offering it upstream are
# the same operation. That only works if a commit upstream has taken *disappears* here, and
# is seen to disappear: `git rebase` drops a commit whose patch-id upstream already has, and
# without this report that drop is silent. A silent drop reads as a patch that went missing.
set -euo pipefail

cd "$(dirname "$0")/.."

TARGET="${1:-}"
if [[ -z "$TARGET" ]]; then
    echo "usage: just element-web-rebase <version>   (e.g. v1.12.25)" >&2
    exit 2
fi

# shellcheck source=../elementium.config.sh
source ./elementium.config.sh

SRC=".element-web-src"

fail() {
    echo "FAILED: $1" >&2
    exit 1
}

# A rebase rewrites history. Doing that over uncommitted work loses it, and the source
# checkout is also a build directory -- so "is anything uncommitted" is a question worth
# asking before touching it rather than after.
if [[ -d "$SRC/.git" ]] && [[ -n "$(git -C "$SRC" status --porcelain)" ]]; then
    echo "The Element Web checkout at $SRC has uncommitted changes:" >&2
    git -C "$SRC" status --short >&2
    fail "refusing to rebase over them. Commit or clean them first."
fi

if [[ ! -d "$SRC/.git" ]]; then
    echo "cloning $ELEMENT_WEB_FORK into $SRC"
    # Not shallow: a shallow clone cannot be rebased onto a tag it does not have.
    git clone "$ELEMENT_WEB_FORK" "$SRC" || fail "could not clone $ELEMENT_WEB_FORK"
fi

# Two remotes, always. GitHub does not copy tags into a fork, so the tag this rebases onto
# exists only upstream; fetching from the fork alone fails with "invalid upstream
# <version>", which reads like a typo rather than a missing remote.
git -C "$SRC" remote get-url upstream >/dev/null 2>&1 ||
    git -C "$SRC" remote add upstream "$ELEMENT_WEB_UPSTREAM"
git -C "$SRC" remote set-url upstream "$ELEMENT_WEB_UPSTREAM"

echo "fetching $TARGET from $ELEMENT_WEB_UPSTREAM"
git -C "$SRC" fetch --quiet upstream "refs/tags/$TARGET:refs/tags/$TARGET" 2>/dev/null ||
    git -C "$SRC" fetch --quiet --tags upstream ||
    fail "could not fetch tags from $ELEMENT_WEB_UPSTREAM"
git -C "$SRC" rev-parse --verify --quiet "refs/tags/$TARGET" >/dev/null ||
    fail "$ELEMENT_WEB_UPSTREAM has no tag $TARGET"

git -C "$SRC" fetch --quiet origin || fail "could not fetch from $ELEMENT_WEB_FORK"

BRANCH="$ELEMENT_WEB_PATCH_BRANCH"
if ! git -C "$SRC" rev-parse --verify --quiet "refs/remotes/origin/$BRANCH" >/dev/null &&
    ! git -C "$SRC" rev-parse --verify --quiet "refs/heads/$BRANCH" >/dev/null; then
    echo "The fork has no branch '$BRANCH', so there is nothing to rebase."
    echo "That is the expected state until the first patch is carried. To start one:"
    echo "  git -C $SRC checkout -b $BRANCH $TARGET"
    exit 0
fi

git -C "$SRC" checkout --quiet "$BRANCH" 2>/dev/null ||
    git -C "$SRC" checkout --quiet -b "$BRANCH" "origin/$BRANCH"

BASE="${ELEMENT_WEB_PATCH_BASE:-}"
if [[ -z "$BASE" ]]; then
    # Nothing recorded, so ask git where the branch left upstream's history. Merge-base
    # against the target is the honest answer when the pin has never been written.
    BASE=$(git -C "$SRC" merge-base "$BRANCH" "$TARGET")
fi

# What we are carrying, before the rebase moves any of it. Recorded here because after a
# successful rebase the commits have new hashes, and a commit that was dropped has none.
mapfile -t CARRIED < <(git -C "$SRC" rev-list --reverse "$BASE..$BRANCH")
if [[ ${#CARRIED[@]} -eq 0 ]]; then
    echo "no patches carried on '$BRANCH'; nothing to rebase"
    exit 0
fi

echo
echo "rebasing ${#CARRIED[@]} patch(es) onto $TARGET"

# `--onto` with an explicit base rather than a plain rebase: the branch's own merge-base
# with the new tag includes every upstream commit between the two releases, and rebasing
# those onto themselves is both slow and meaningless.
set +e
REBASE_OUT=$(git -C "$SRC" rebase --onto "$TARGET" "$BASE" "$BRANCH" 2>&1)
REBASE_RC=$?
set -e

report_commit() {
    local sha="$1" outcome="$2" note="${3:-}"
    local subject
    subject=$(git -C "$SRC" log -1 --format=%s "$sha")
    printf '  %-10s %s  %s\n' "$outcome" "${sha:0:7}" "$subject"
    [[ -n "$note" ]] && printf '             ^ %s\n' "$note"
    return 0
}

patch_id() {
    git -C "$SRC" show "$1" | git patch-id --stable | cut -d' ' -f1
}

# The patch-ids of everything currently sitting on top of `$TARGET`.
#
# Read fresh each time rather than once, because in the conflict path this is called while
# the rebase is paused part-way through and the set grows as commits replay.
replayed_patch_ids() {
    local sha
    for sha in $(git -C "$SRC" rev-list "$TARGET..HEAD" 2>/dev/null); do
        patch_id "$sha"
    done
}

# Say what became of each carried commit, up to `$1` commits (all of them if unset).
#
# `dropped` is decided by patch-id, never by position. Reporting a commit as applied
# because the rebase got past it is exactly the silent drop this whole report exists to
# prevent: a commit upstream has taken *vanishes* here, and that vanishing is the signal
# that a contribution landed.
report_carried() {
    local limit="${1:-${#CARRIED[@]}}" i=0 sha pid
    local replayed
    replayed=$(replayed_patch_ids)
    for sha in "${CARRIED[@]}"; do
        [[ $i -ge $limit ]] && break
        i=$((i + 1))
        pid=$(patch_id "$sha")
        if grep -qx "$pid" <<<"$replayed"; then
            report_commit "$sha" applied
        else
            DROPPED=$((DROPPED + 1))
            report_commit "$sha" dropped "upstream has this. If it had an open PR, it merged."
        fi
    done
}

DROPPED=0

if [[ $REBASE_RC -ne 0 ]]; then
    # Which commit it stopped on. A conflict is not a failure of this script; it is the
    # answer, and the most likely cause deserves saying out loud.
    STOPPED=$(git -C "$SRC" rev-parse --verify --quiet REBASE_HEAD || true)
    # Everything before the one it stopped on, classified by patch-id like any other run.
    BEFORE=0
    for sha in "${CARRIED[@]}"; do
        [[ -n "$STOPPED" ]] && [[ "$sha" == "$STOPPED" ]] && break
        BEFORE=$((BEFORE + 1))
    done
    report_carried "$BEFORE"
    if [[ -n "$STOPPED" ]]; then
        note="upstream may have taken this with changes made in review."
        if git -C "$SRC" log -1 --format=%B "$STOPPED" | grep -q "Elementium-Upstream:"; then
            note+=$'\n               Compare before resolving; `git rebase --skip` if it is theirs now.'
        fi
        report_commit "$STOPPED" conflicted "$note"
    fi
    echo
    echo "The rebase is paused in $SRC. Resolve and \`git rebase --continue\`,"
    echo "or \`git rebase --skip\` if upstream now has this change."
    echo "$REBASE_OUT" | tail -5 >&2
    exit 1
fi

# Rebase succeeded. A carried commit is "dropped" when nothing on the new branch has its
# patch-id -- which is how a contribution landing upstream is discovered, and the reason
# this report exists at all.
mapfile -t NOW < <(git -C "$SRC" rev-list --reverse "$TARGET..$BRANCH")
report_carried

echo
echo "  ${#NOW[@]} patch(es) now carried against $TARGET; $DROPPED dropped"
echo
echo "Record the new base by setting ELEMENT_WEB_PATCH_BASE=$TARGET in elementium.config.sh,"
echo "then push the rebased branch when you are satisfied with it:"
echo "  git -C $SRC push --force-with-lease origin $BRANCH"
