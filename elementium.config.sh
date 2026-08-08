#!/usr/bin/env bash
# Elementium — Element Web source configuration
# This file is sourced by build scripts. Override any variable via environment.

# How to obtain Element Web: "release" (download tarball) or "git" (clone + build)
: "${ELEMENT_WEB_SOURCE:=release}"

# Release mode settings
: "${ELEMENT_WEB_VERSION:=v1.12.25}"

# Git repo (used by both release and git modes for the download/clone URL)
: "${ELEMENT_WEB_REPO:=https://github.com/element-hq/element-web}"

# Git mode settings — branch/tag to check out (only used when SOURCE=git)
: "${ELEMENT_WEB_BRANCH:=}"

# --- Carrying patches against Element Web -------------------------------------------
#
# Where changes to Element Web itself live: one atomic commit per change on a long-lived
# branch of a fork, rebased onto each upstream tag. Carrying a change and offering it
# upstream are then the same operation -- `git cherry-pick` onto a branch from the current
# tag, and a pull request -- and a change upstream accepts disappears from the branch at the
# next rebase, because rebase drops commits whose patch-id upstream already has.
#
# Public deliberately, not by default: Element Web is AGPL-3.0 and this project declares
# AGPL-3.0-or-later, so a shipped source patch is a modified AGPL work whose corresponding
# source has to be available to whoever receives it.
#
# The fork already existed and is in sync with upstream `develop`, with no divergent
# branches, so the patch branch starts from a clean base.
: "${ELEMENT_WEB_FORK:=https://github.com/alisonjenkins/element-web}"

# Where tags come from, which is *not* the fork.
#
# GitHub does not copy tags into a fork, and the fork has none. The patch branch rebases
# onto upstream release tags, so a rebase that fetched only from the fork would fail with
# "invalid upstream v1.12.25" -- which reads like a typo rather than a missing remote. Two
# remotes: the fork for the branch, `ELEMENT_WEB_REPO` above for the tags.
: "${ELEMENT_WEB_UPSTREAM:=${ELEMENT_WEB_REPO}}"

# The branch carrying our patches. Rebased, never merged: merging would bury the individual
# commits that make a contribution a cherry-pick rather than a rewrite.
: "${ELEMENT_WEB_PATCH_BRANCH:=elementium}"

# The upstream tag the patch branch is currently rebased onto. Kept beside the pin above
# rather than derived, so "which upstream are the patches against" is answerable without a
# checkout -- and so a mismatch with ELEMENT_WEB_VERSION is visible rather than latent.
: "${ELEMENT_WEB_PATCH_BASE:=v1.12.25}"

# Override examples:
#
# Build from a custom fork/branch:
#   ELEMENT_WEB_SOURCE="git"
#   ELEMENT_WEB_REPO="https://github.com/ali/element-web"
#   ELEMENT_WEB_BRANCH="my-feature"
#
# Use a specific release tag:
#   ELEMENT_WEB_SOURCE="release"
#   ELEMENT_WEB_VERSION="v1.12.10"
