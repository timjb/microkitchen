default: test

build:
    cargo build --workspace

# Unit tests and VM-free integration tests.
test:
    cargo test --workspace

# Tests that boot microVMs (need KVM and msb).
test-integration:
    MK_TEST_ISOLATE_HOME=1 cargo nextest run --workspace --run-ignored=only --test-threads 2

lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
