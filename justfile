
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

# Install the /offload-onboard skill for Claude Code
install-skill:
    #!/usr/bin/env bash
    set -euo pipefail
    skill_src="$(just _repo-root)/skills/offload-onboard"
    skill_dst="$HOME/.claude/skills/offload-onboard"
    mkdir -p "$HOME/.claude/skills"
    if [ -L "$skill_dst" ]; then
        echo "Updating existing symlink..."
        rm "$skill_dst"
    elif [ -e "$skill_dst" ]; then
        echo "Error: $skill_dst already exists and is not a symlink. Remove it manually."
        exit 1
    fi
    ln -s "$skill_src" "$skill_dst"
    echo "Installed: $skill_dst -> $skill_src"
    echo "You can now use /offload-onboard in any repository."

ratchets:
    ratchets check

_repo-root:
    @git rev-parse --show-toplevel
