#!/usr/bin/env bash
# Elementium — Element Web source configuration
# This file is sourced by build scripts. Override any variable via environment.

# How to obtain Element Web: "release" (download tarball) or "git" (clone + build)
: "${ELEMENT_WEB_SOURCE:=release}"

# Release mode settings
: "${ELEMENT_WEB_VERSION:=v1.12.11}"

# Git repo (used by both release and git modes for the download/clone URL)
: "${ELEMENT_WEB_REPO:=https://github.com/element-hq/element-web}"

# Git mode settings — branch/tag to check out (only used when SOURCE=git)
: "${ELEMENT_WEB_BRANCH:=}"

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
