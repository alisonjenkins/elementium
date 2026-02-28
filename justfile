# Elementium task runner

# Development
dev:
    cargo tauri dev

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
