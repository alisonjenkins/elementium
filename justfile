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
# window appears.
#
# THIS USES THE CAMERA AND MICROPHONE. Element Call acquires both in its lobby, before
# any control can decline them, so joining at all opens the webcam -- the light comes on.
# ELEMENTIUM_AUTOJOIN_VIDEO=0 strips video from the request instead.
#
# Logs land in /tmp/elementium.log as usual.
app-join:
    cd test-env && ./configure-synapse.sh
    cd test-env && docker compose up -d
    cd test-env && ./provision.sh > ../target/test-env-fixture.json
    cd frontend && pnpm exec vite build -c vite.shims.config.ts
    cd frontend && pnpm exec vite build -c vite.autojoin.config.ts
    # The env has to reach `cargo tauri dev`, not just the patch above: tauri runs
    # `prepare-dev.sh` -- which calls the same patch script -- before starting. Without it
    # that run sees no ELEMENTIUM_TEST_ENV and no ELEMENTIUM_AUTOJOIN, so it restores the
    # production config and removes the autojoin driver, and the app comes up pointed at a
    # real homeserver doing nothing. Which is exactly what happened the first time.
    ELEMENTIUM_TEST_ENV=1 ELEMENTIUM_AUTOJOIN=1 \
        ELEMENTIUM_AUTOJOIN_VIDEO="${ELEMENTIUM_AUTOJOIN_VIDEO:-1}" \
        nix shell nixpkgs#xvfb-run --command xvfb-run -a -s "-screen 0 1280x800x24" \
            cargo tauri dev

# Stop the local MatrixRTC stack.
test-env-down:
    cd test-env && docker compose down

# Browser tests. Bring the stack up and tear it down around the run themselves.
test-browser:
    cd frontend && pnpm exec playwright test

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
