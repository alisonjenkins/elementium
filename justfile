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
    cd test-env && docker compose up -d
    cd test-env && ./provision.sh
    ELEMENTIUM_TEST_ENV=1 cargo tauri dev

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
