
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
    cargo run -- -c offload-cargo-local.toml run {{args}}

# Pytest targets tolerate exit code 2 (all tests passed but some were flaky)
test-pytest-local args="":
    cargo run -- -c offload-pytest-local.toml run {{args}} || [ $? -eq 2 ]

test-cargo-modal args="":
    cargo run -- -c offload-cargo-modal.toml run {{args}}

test-pytest-modal args="":
    cargo run -- -c offload-pytest-modal.toml run {{args}} || [ $? -eq 2 ]

test-cargo-default args="":
    cargo run -- -c offload-cargo-default.toml run {{args}}

test-pytest-default args="":
    cargo run -- -c offload-pytest-default.toml run {{args}} || [ $? -eq 2 ]

# Install all Offload skills for Claude Code
install-skills:
    ./install-skills.sh

# Alias for backward compatibility
install-skill: install-skills

ratchets:
    ratchets check
