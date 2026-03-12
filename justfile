
default: help

help:
    @just --list

# Install binary to cargo bin path
install:
    cargo install --path .

# Build the project
build:
    cargo build

# Run unit tests directly via cargo
test:
    cargo nextest run

# Offload test targets: test-{framework}-{provider}
test-cargo-local args="":
    cargo run -- -c offload-cargo-local.toml {{args}} run

# Pytest targets tolerate exit code 2 (all tests passed but some were flaky)
test-pytest-local args="":
    cargo run -- -c offload-pytest-local.toml {{args}} run || [ $? -eq 2 ]

test-cargo-modal args="":
    cargo run -- -c offload-cargo-modal.toml {{args}} run

test-pytest-modal args="":
    cargo run -- -c offload-pytest-modal.toml {{args}} run || [ $? -eq 2 ]

test-cargo-default args="":
    cargo run -- -c offload-cargo-default.toml {{args}} run

test-pytest-default args="":
    cargo run -- -c offload-pytest-default.toml {{args}} run || [ $? -eq 2 ]

# Install all Offload skills for Claude Code
install-skills:
    ./install-skills.sh

# Alias for backward compatibility
install-skill: install-skills

ratchets:
    ratchets check
