default: test

build:
    cargo build --workspace

# Unit tests and VM-free integration tests.
test:
    cargo test --workspace

# The guest attribution script (scripts/guest/whodial.sh) against fake /proc trees.
test-scripts:
    cargo test -p microkitchen --lib broker::attribution

# Tests that boot microVMs (need KVM and msb).
test-integration:
    MK_TEST_ISOLATE_HOME=1 cargo nextest run --workspace --run-ignored=only --test-threads 2

# The VM tests without cargo-nextest, one at a time, against the user's microsandbox home.
test-vm:
    cargo test --workspace -- --ignored --test-threads 1

lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

# What CI runs on hosted runners.
check: lint test

# Install the microkitchen binary into ~/.cargo/bin.
install:
    cargo install --locked --path crates/microkitchen

# Serve the documentation site with live reload.
docs:
    mise run docs:dev
