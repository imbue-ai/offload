default: help

help:
    @just --list

# Install binary to cargo bin path
install:
    cargo install --path .

# Build the project
build:
    cargo build

# Run tests
test:
    cargo nextest run

test-modal:
    cargo run -- -c offload-modal.toml run

test-cargo-modal:
    cargo run -- -c offload-cargo-modal.toml run
