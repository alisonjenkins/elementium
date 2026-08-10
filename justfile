# Elementium task runner

# List justfile targets
list:
    @just --list

# Development
dev:
    cargo tauri dev

# Run against the local MatrixRTC stack in test-env/ rather than a real homeserver.
#
# Brings the stack up, points Element Web at it, and creates participants. Use this
# when reproducing a fault that needs more than one person in the call: log in as
# tester1 (password test-password-1) and let Playwright drive the rest.
#
# The stack is left running afterwards -- `just test-env-down` stops it.
dev-test-env:
    cd test-env && ./configure-synapse.sh
    cd test-env && docker compose up -d
    cd test-env && ./provision.sh
    ELEMENTIUM_TEST_ENV=1 cargo tauri dev

# Put other people in a call and leave them there, so Elementium can join them.
#
# Every Element Call scenario tested here passes, which leaves one configuration
# uncovered: Elementium as a participant. Playwright cannot drive it -- it is a Tauri
# app with a native WebRTC stack -- so this supplies the other side instead.
#
# Run this in one terminal and `just dev-test-env` in another, then log in as
# tester1 (test-password-1) and join the call. Ctrl-C to stop.
call-peers:
    cd frontend && ELEMENTIUM_HOLD_PEERS=1 pnpm exec playwright test \
        tests/matrixrtc/peers.spec.ts --reporter=list --timeout=0 --workers=1

# Run Elementium headless and have it join the call, for testing.
#
# The other half of `just call-peers`: that puts real participants in a call, this puts
# Elementium in the same one without anyone clicking. Runs on a virtual display, so no
# window appears -- GDK_BACKEND=x11 and an empty WAYLAND_DISPLAY are what make that true:
# GTK prefers Wayland when WAYLAND_DISPLAY is set and ignores the Xvfb display entirely, so
# without them the window opens on the real desktop.
#
# THIS USES THE CAMERA AND MICROPHONE. Element Call acquires both in its lobby, before
# any control can decline them, so joining at all opens the webcam -- the light comes on.
# ELEMENTIUM_AUTOJOIN_VIDEO=0 strips video from the request instead.
#
# Logs land in /tmp/elementium.log as usual.
app-join:
    # Skipped when the caller has already established the stack -- `just test-app-call` has,
    # through Playwright's global setup, which also decides whether it is allowed to tear the
    # stack down afterwards. Doing it again from here is not the no-op it looks like: the
    # containers have fixed names, so a `docker compose up -d` run from a different directory
    # (a worktree, say) recreates them against *that* directory's empty bind mount and takes
    # the homeserver down mid-test. What the test then sees is the homeserver answering HTML
    # 502s to a login, which reads as anything but "something restarted synapse".
    [ "${ELEMENTIUM_STACK_READY:-0}" = "1" ] || (cd test-env && ./configure-synapse.sh)
    [ "${ELEMENTIUM_STACK_READY:-0}" = "1" ] || (cd test-env && docker compose up -d)
    # Only if there is no fixture yet. `provision.sh` creates a *new* room every run, so
    # re-provisioning here would put the app in a different room from the participants
    # `just call-peers` already has in a call -- which looks exactly like a call that does
    # not connect.
    [ -f target/test-env-fixture.json ] || (cd test-env && ./provision.sh > ../target/test-env-fixture.json)
    cd frontend && pnpm exec vite build -c vite.shims.config.ts
    # The env has to reach `cargo tauri dev`, not just the patch above: tauri runs
    # `prepare-dev.sh` -- which calls the same patch script -- before starting. Without it
    # that run sees no ELEMENTIUM_TEST_ENV and no ELEMENTIUM_AUTOJOIN, so it restores the
    # production config and removes the autojoin driver, and the app comes up pointed at a
    # real homeserver doing nothing. Which is exactly what happened the first time.
    # A throwaway profile, via XDG_DATA_HOME. Without it the webview uses the real one in
    # ~/.local/share/io.github.elementium, and the autojoin's test session lands on top of
    # whatever account is already signed in there. That produced "Unable to restore session"
    # -- Element Web finding a token and device from the test homeserver against a crypto
    # store belonging to a different account, and refusing rather than corrupting it. The
    # offered remedy is "Clear Storage and Sign Out", which destroys encrypted history.
    #
    # Nothing this recipe does should be able to reach a real session, so it does not share
    # a directory with one.
    # Wiped each run. The profile keeps a crypto store, and the fixture hands out a new
    # device on every provision -- a new device against a store belonging to an older one is
    # a client the server has keys for that it can no longer use, which presents as the room
    # never loading.
    rm -rf target/app-join-profile
    mkdir -p target/app-join-profile
    ELEMENTIUM_TEST_ENV=1 ELEMENTIUM_AUTOJOIN=1 \
        ELEMENTIUM_AUTOJOIN_VIDEO="${ELEMENTIUM_AUTOJOIN_VIDEO:-1}" \
        ELEMENTIUM_AUTOJOIN_SCREENSHARE="${ELEMENTIUM_AUTOJOIN_SCREENSHARE:-0}" \
        XDG_DATA_HOME="$PWD/target/app-join-profile/data" \
        XDG_CONFIG_HOME="$PWD/target/app-join-profile/config" \
        XDG_CACHE_HOME="$PWD/target/app-join-profile/cache" \
        GDK_BACKEND=x11 WAYLAND_DISPLAY= \
        nix shell nixpkgs#xvfb-run --command xvfb-run -a -s "-screen 0 1280x800x24" \
            cargo tauri dev

# A whole call, with Elementium in it, asserted on by nobody.
#
# Every fault so far was found by the maintainer joining a call with real friends and
# describing what they saw. This is that evening as a command: it brings up the local
# MatrixRTC stack if it is not already running, puts a real Element Web participant in a
# call, has Elementium join it by itself on a virtual display, measures what each end
# actually decodes, and stops what it started. Stacks it did not start are left alone.
#
# THIS USES THE CAMERA AND MICROPHONE, for the reason `just app-join` gives: Element Call
# takes both in its lobby, before any control to decline them exists. The GUI itself is
# headless -- Xvfb, via `just app-join`.
#
# The application is built first, deliberately: inside the test that build would be
# indistinguishable from a call taking twelve minutes to connect.
#
# All four assertions pass as of 2026-08-09, including the late-joiner one that was written
# expecting to fail -- see the comment on it in app-call.spec.ts for what that measured and
# what it does *not* cover.
test-app-call:
    cargo build -p elementium
    cd frontend && ELEMENTIUM_APP_CALL=1 pnpm exec playwright test \
        tests/matrixrtc/app-call.spec.ts --reporter=list --workers=1

# Is the audio that comes out the other end the audio that went in?
#
# `just test-app-call` measures that audio flows. This measures that it is the *same audio*:
# every participant transmits a ladder of six pure tones cycling in a fixed order, each
# participant's ladder drawn from a different set of frequencies, and each end asserts that
# the tones it received are the right ones, in the right order, with no gap over 300ms.
# Nothing else here can tell a voice from a chainsaw -- packets, samples and decoded frames
# all read healthy for the fault this exists to find.
#
# Nothing on this machine is reconfigured: no default is changed, no sound-server module is
# loaded and no microphone is opened. Elementium plays the generated signal as its microphone
# (ELEMENTIUM_FAKE_MIC) and its own playback is silenced by an ALSA config that applies to
# that one process, so the room can neither be heard nor transmitted. The camera is not used
# either -- this joins audio-only.
#
# Three participants: Elementium and two Element Web browsers, so a fault that only appears
# when the SFU forwards to several is in range.
test-app-call-audio:
    cargo build -p elementium
    cd frontend && ELEMENTIUM_APP_CALL_AUDIO=1 pnpm exec playwright test \
        tests/matrixrtc/app-call-audio.spec.ts --reporter=list --workers=1

# The same call, with a crowd in it before Elementium arrives.
#
# The twenty-frames-a-minute fault has never reproduced against a single peer, and one
# difference between this harness and the calls where it does reproduce is simply how many
# people are in them -- the point at which an SFU stops relaying and starts choosing what to
# forward to whom. Every assertion is the same; only the population changes.
#
# If this fails where `test-app-call` passes, the fault is reproducible on demand and there is
# nothing left to guess about.
test-app-call-crowd peers="3":
    cargo build -p elementium
    # The fixture is removed so the stack re-provisions with enough testers. Logging in as
    # one `provision.sh` never made gets as far as Element Web and then stops at a "Verify
    # this device" dialog, which surfaces two minutes later as a missing message composer.
    rm -f target/test-env-fixture.json
    cd frontend && ELEMENTIUM_APP_CALL=1 ELEMENTIUM_APP_CALL_PEERS={{peers}} \
        ELEMENTIUM_TEST_PARTICIPANTS=$(({{peers}} + 2)) \
        pnpm exec playwright test tests/matrixrtc/app-call.spec.ts --reporter=list --workers=1

# The ordinary call, recording what the *real* microphone path produces.
#
# `just test-app-call-audio` is the stronger test and should be preferred: it transmits a
# known signal and checks the far end received that signal. This one exists for the case that
# cannot cover -- a real microphone in a real room, where the question is what a human voice
# and the gain applied to it actually sound like. It writes the three capture points and
# converts them to WAV for listening.
test-app-call-dumps:
    cargo build -p elementium
    rm -f /tmp/elementium_audio_dump_*.f32le /tmp/elementium_audio_dump_*.wav
    cd frontend && ELEMENTIUM_APP_CALL=1 ELEMENTIUM_AUDIO_DUMP=1 \
        pnpm exec playwright test tests/matrixrtc/app-call.spec.ts --reporter=list --workers=1
    just audio-dumps

# Move to an Element Web release, and find out whether we still work on it.
#
# Fetches the version, rebuilds and re-injects the shims, and runs the shim contract checks
# in a real browser. Writes the pin only if all of that passes -- a half-applied upgrade is
# worse than none, because the next person cannot tell which version they are debugging.
#
# It answers "do the shims install", not "do calls work". The second needs media, and the
# media check is `just call-peers` plus `just app-join`; see
# specs/007-element-web-upgrade/quickstart.md.
element-web-sync version:
    ./scripts/element-web-sync.sh {{version}}

# Move the patch branch onto a new upstream tag, and say what happened to each commit.
#
# Three outcomes per commit, not two: applied, conflicted, and **dropped**. Dropped means
# upstream has the change now -- `git rebase` discards a commit whose patch-id upstream
# already has -- which is how a contribution landing is discovered. It has to be stated,
# because silently vanishing reads as a patch that went missing.
element-web-rebase version:
    ./scripts/element-web-rebase.sh {{version}}

# Turn a carried commit into a branch that can be opened as a pull request.
#
# Branches from the upstream tag the patches are rebased onto and cherry-picks the one
# commit. Never pushes -- the remote is someone's account, and Element Web requires a CLA
# signed personally before a pull request can merge.
element-web-pr commit:
    ./scripts/element-web-pr.sh {{commit}}

# Regenerate `element-web-patches.md` from the patch branch.
#
# Generated, never hand-written: a hand-written list is true the day it is written and
# quietly stops being true the first time upstream takes one of the patches.
element-web-patches:
    ./scripts/element-web-patches.sh

# Run the whole carry-and-contribute cycle against a synthetic upstream.
#
# Carries a change, offers it, has a stand-in upstream take it, and checks it *disappears*
# on the next rebase with nobody editing anything. That drop is the claim the arrangement
# rests on, and it is the one thing reading the scripts cannot verify.
element-web-patch-selftest:
    ./scripts/element-web-patch-selftest.sh

# Leave and forget every room the test participants are in.
#
# Each provision creates a room and each call test creates another, so they pile up. The
# Playwright suite does this for itself before each run; this is for clearing up by hand.
test-env-clean:
    cd test-env && ./cleanup-rooms.sh

# Stop the local MatrixRTC stack.
test-env-down:
    cd test-env && docker compose down

# Browser tests. Bring the stack up and tear it down around the run themselves.
test-browser:
    cd frontend && pnpm exec playwright test

# Unit tests for the pure parts of the shim. No homeserver, no browser, sub-second.
test-frontend:
    cd frontend && pnpm exec vitest run

# Build release
build:
    cargo tauri build

# Run all tests
test:
    cargo test --workspace

# Run tests for a specific crate
test-crate crate:
    cargo test -p {{crate}}

# Check all code compiles
check:
    cargo check --workspace

# Run clippy lints
lint:
    cargo clippy --workspace -- -D warnings

# Format code
fmt:
    cargo fmt --all

# Format check
fmt-check:
    cargo fmt --all -- --check

# Install frontend dependencies
frontend-install:
    cd frontend && pnpm install

# Run frontend dev server
frontend-dev:
    cd frontend && pnpm dev

# Build frontend
frontend-build:
    cd frontend && pnpm build

# Clean build artifacts
clean:
    cargo clean
    rm -rf frontend/dist frontend/node_modules

# Enter nix dev shell
shell:
    nix develop

# Turn the raw audio dumps into WAV files you can play.
#
# `ELEMENTIUM_AUDIO_DUMP=1` (or `touch /tmp/ELEMENTIUM_AUDIO_DUMP`, which needs no restart)
# makes the capture path write headerless f32 at three points: `capture-raw` is what the
# microphone produced, `capture-encoder-in` is the frame handed to Opus after gain and
# resampling, and `capture-loopback` is our own encoder's output decoded back -- which is
# the closest thing to what the far end actually hears.
#
# Listen to those three in order and the question "is the bad audio ours or theirs" answers
# itself, which no amount of reading counters has managed.
audio-dumps:
    #!/usr/bin/env bash
    set -euo pipefail
    shopt -s nullglob
    found=0
    for f in /tmp/elementium_audio_dump_*.f32le; do
        found=1
        # The rate and channel count are in the name precisely so this can be automatic.
        base="$(basename "$f" .f32le)"
        rate="$(sed -n 's/.*_\([0-9]\+\)hz_.*/\1/p' <<<"$base")"
        ch="$(sed -n 's/.*_\([0-9]\+\)ch$/\1/p' <<<"$base")"
        if [ -z "$rate" ] || [ -z "$ch" ]; then
            echo "skipping $f: no rate/channels in the name (dump predates this format)" >&2
            continue
        fi
        ffmpeg -loglevel error -y -f f32le -ar "$rate" -ac "$ch" -i "$f" "/tmp/$base.wav"
        echo "$(du -h "/tmp/$base.wav" | cut -f1)	/tmp/$base.wav"
    done
    if [ "$found" = 0 ]; then
        echo "No dumps in /tmp. Set ELEMENTIUM_AUDIO_DUMP=1 or touch /tmp/ELEMENTIUM_AUDIO_DUMP, then make a call." >&2
    fi
