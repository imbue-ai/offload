#!/bin/bash
# Project-specific setup for the PR review bot on offload.
# Installs toolchains and test dependencies that ci.yml needs.
# Runs in the GitHub Actions ubuntu-latest environment.

set -euo pipefail

# Rust stable
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
source "$HOME/.cargo/env"

# Install cargo tools via prebuilt binaries (much faster than cargo install)
# cargo-nextest
curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C "$HOME/.cargo/bin"

# cargo-deny
VERSION="0.16.4"
curl -LsSf "https://github.com/EmbarkStudios/cargo-deny/releases/download/$VERSION/cargo-deny-$VERSION-x86_64-unknown-linux-musl.tar.gz" \
  | tar zxf - --strip-components=1 -C "$HOME/.cargo/bin"

# ratchets
cargo install ratchets --version 0.2.6

# just
curl --proto '=https' --tlsv1.2 -sSf https://just.systems/install.sh | bash -s -- --to "$HOME/.cargo/bin"

# uv (Python package manager)
curl -LsSf https://astral.sh/uv/install.sh | sh
