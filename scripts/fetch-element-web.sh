#!/usr/bin/env bash
# Downloads or builds Element Web based on elementium.config.sh settings.
# Idempotent — skips if the current version/commit already matches.
set -euo pipefail

cd "$(dirname "$0")/.."

# Source configuration (env vars take precedence via := defaults in config)
# shellcheck source=../elementium.config.sh
source ./elementium.config.sh

DIST_DIR="element-web-dist"
SOURCE_INFO="$DIST_DIR/.source-info"

fetch_release() {
    local version="$ELEMENT_WEB_VERSION"
    local tarball="element-${version}.tar.gz"
    local url="${ELEMENT_WEB_REPO}/releases/download/${version}/${tarball}"

    # Check if already fetched
    if [[ -f "$SOURCE_INFO" ]]; then
        local existing
        existing=$(cat "$SOURCE_INFO")
        if [[ "$existing" == "release:${ELEMENT_WEB_REPO}:${version}" ]]; then
            echo "[fetch] Element Web ${version} already present, skipping."
            return
        fi
    fi

    echo "[fetch] Downloading Element Web ${version} from ${url}..."
    rm -rf "$DIST_DIR"
    mkdir -p "$DIST_DIR"

    local tmpfile
    tmpfile=$(mktemp)

    if ! curl -fSL "$url" -o "$tmpfile"; then
        rm -f "$tmpfile"
        echo "[fetch] ERROR: Download failed." >&2
        exit 1
    fi

    tar xzf "$tmpfile" --strip-components=1 -C "$DIST_DIR"
    rm -f "$tmpfile"

    echo "release:${ELEMENT_WEB_REPO}:${version}" > "$SOURCE_INFO"
    echo "[fetch] Element Web ${version} downloaded to ${DIST_DIR}/"
}

fetch_git() {
    local repo="$ELEMENT_WEB_REPO"
    local branch="${ELEMENT_WEB_BRANCH:-develop}"
    local cache_dir=".element-web-src"

    # Check remote HEAD
    local remote_sha
    remote_sha=$(git ls-remote "$repo" "refs/heads/$branch" | cut -f1)
    if [[ -z "$remote_sha" ]]; then
        # Try as a tag
        remote_sha=$(git ls-remote "$repo" "refs/tags/$branch" | cut -f1)
    fi

    if [[ -z "$remote_sha" ]]; then
        echo "[fetch] ERROR: Could not resolve branch/tag '$branch' in $repo" >&2
        exit 1
    fi

    # Check if already built from this commit
    if [[ -f "$SOURCE_INFO" ]]; then
        local existing
        existing=$(cat "$SOURCE_INFO")
        if [[ "$existing" == "git:${repo}:${branch}:${remote_sha}" ]]; then
            echo "[fetch] Element Web (git: ${branch}@${remote_sha:0:8}) already built, skipping."
            return
        fi
    fi

    echo "[fetch] Building Element Web from ${repo} branch ${branch}..."

    # Clone or update cache
    if [[ -d "$cache_dir/.git" ]]; then
        git -C "$cache_dir" fetch origin "$branch"
        git -C "$cache_dir" checkout FETCH_HEAD
    else
        rm -rf "$cache_dir"
        # Not shallow. A shallow clone cannot be rebased onto a different upstream tag,
        # which is the whole point of the patch branch; and the build stamps its version
        # from `git describe`, which needs tags.
        git clone --branch "$branch" "$repo" "$cache_dir"
    fi

    # Build, following upstream's own CI rather than inferring from package.json.
    #
    # Element Web became an nx/pnpm monorepo (`apps/web`, `apps/desktop`, `packages/*`), and
    # the three lines that used to be here were wrong on every count: yarn (there is no
    # yarn.lock), a root `build` script (the root has 18 scripts and none of them is
    # `build`), and `webapp/` at the top level. Reading package.json is what produced that
    # mistake -- the entry point is a workspace directory away. `.github/workflows/build.yml`
    # is the authority, because it is what upstream actually runs.
    #
    # `scripts/layered.sh` runs first and is not optional: a build that skips it is not the
    # build upstream ships.
    (
        cd "$cache_dir"
        ./scripts/layered.sh
        cp apps/web/element.io/develop/config.json apps/web/config.json
        # CI_PACKAGE and VERSION are what upstream sets; without VERSION the build stamps
        # itself from git describe and fails on a shallow clone.
        cd apps/web
        CI_PACKAGE=true VERSION="$(../../scripts/get-version-from-git.sh 2>/dev/null || echo "$branch")" \
            pnpm run build
    )

    # `{projectRoot}/webapp` per apps/web/project.json -- the directory was never renamed,
    # only moved.
    local built="$cache_dir/apps/web/webapp"
    [[ -d "$built" ]] || {
        echo "[fetch] ERROR: build produced no $built" >&2
        exit 1
    }

    # Copy built output
    rm -rf "$DIST_DIR"
    cp -r "$built" "$DIST_DIR"

    echo "git:${repo}:${branch}:${remote_sha}" > "$SOURCE_INFO"
    echo "[fetch] Element Web built from ${branch}@${remote_sha:0:8}"
}

case "$ELEMENT_WEB_SOURCE" in
    release) fetch_release ;;
    git)     fetch_git ;;
    *)
        echo "[fetch] ERROR: Unknown ELEMENT_WEB_SOURCE='$ELEMENT_WEB_SOURCE' (expected 'release' or 'git')" >&2
        exit 1
        ;;
esac
