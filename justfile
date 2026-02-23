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

_repo-root:
    @git rev-parse --show-toplevel
