#!/usr/bin/env bash
set -euo pipefail

SKILLS=("offload" "offload-onboard")
GITHUB_BASE="https://raw.githubusercontent.com/imbue-ai/offload/main/skills"

# Resolve target skills directory
SKILLS_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/skills"

# Detect whether we are running from within the repo or standalone (curl | bash)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-}")" 2>/dev/null && pwd)" || SCRIPT_DIR=""

if [ -n "$SCRIPT_DIR" ] && [ -d "$SCRIPT_DIR/skills" ]; then
    # In-repo mode: create symlinks for live editing
    echo "Detected in-repo run. Installing skills via symlinks..."
    mkdir -p "$SKILLS_DIR"

    for skill in "${SKILLS[@]}"; do
        src="$SCRIPT_DIR/skills/$skill"
        dst="$SKILLS_DIR/$skill"

        if [ -L "$dst" ]; then
            echo "Updating existing symlink for $skill..."
            rm "$dst"
        elif [ -e "$dst" ]; then
            echo "Error: $dst already exists and is not a symlink. Remove it manually."
            exit 1
        fi

        ln -s "$src" "$dst"
        echo "Installed: $dst -> $src"
    done
else
    # Standalone mode: download SKILL.md files from GitHub
    echo "Standalone mode. Downloading skills from GitHub..."
    mkdir -p "$SKILLS_DIR"

    for skill in "${SKILLS[@]}"; do
        dst="$SKILLS_DIR/$skill"

        if [ -L "$dst" ]; then
            echo "Removing existing symlink for $skill..."
            rm "$dst"
        elif [ -e "$dst" ]; then
            echo "Error: $dst already exists and is not a symlink. Remove it manually."
            exit 1
        fi

        mkdir -p "$dst"
        echo "Downloading $skill/SKILL.md..."
        curl -fsSL "$GITHUB_BASE/$skill/SKILL.md" -o "$dst/SKILL.md"
        echo "Installed: $dst/SKILL.md"
    done
fi

echo "Done. Skills installed to $SKILLS_DIR"
