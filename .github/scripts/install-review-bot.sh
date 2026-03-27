#!/bin/bash
# install-review-bot.sh — One-command setup for the PR review bot.
#
# Run from inside any git repo, or pass owner/repo as an argument.
# Writes all workflow files and sets all secrets.
#
# Usage:
#   cd ~/my-project && path/to/install-review-bot.sh
#   path/to/install-review-bot.sh owner/repo

set -euo pipefail

# --- Resolve repo ---
if [ -n "${1:-}" ]; then
  REPO="$1"
  REPO_DIR=""
else
  REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)
  REPO_DIR=$(git rev-parse --show-toplevel 2>/dev/null || true)
  if [ -z "$REPO" ]; then
    echo "Error: not in a git repo and no repo argument provided."
    echo "Usage: $0 [owner/repo]"
    exit 1
  fi
fi

echo ""
echo "╔══════════════════════════════════════════════════╗"
echo "║  PR Review Bot — Installing to $REPO"
echo "╚══════════════════════════════════════════════════╝"
echo ""

# --- Write files ---
if [ -n "$REPO_DIR" ]; then
  echo "━━━ Writing workflow files ━━━"
  mkdir -p "$REPO_DIR/.github/workflows"
  mkdir -p "$REPO_DIR/.github/scripts"

  # --- pr-review-bot.yml ---
  cat > "$REPO_DIR/.github/workflows/pr-review-bot.yml" << 'WORKFLOW_EOF'
name: PR Review Bot

on:
  pull_request_review_comment:
    types: [created]

jobs:
  handle-review-comment:
    if: >-
      !endsWith(github.event.comment.user.login, '[bot]') &&
      github.event.comment.user.login != 'github-actions'
    runs-on: ubuntu-latest
    permissions:
      contents: write
      pull-requests: write
      actions: read
    concurrency:
      group: pr-review-bot-${{ github.event.pull_request.number }}
      cancel-in-progress: false

    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ github.event.pull_request.head.ref }}
          repository: ${{ github.event.pull_request.head.repo.full_name }}
          token: ${{ secrets.PAT_TOKEN }}

      - uses: actions/setup-python@v5
        with:
          python-version: "3.12"

      # Run project-specific setup if it exists (toolchains, test deps, etc.)
      - name: Project setup
        run: |
          if [ -x .github/review-bot-setup.sh ]; then
            echo "Running project-specific setup..."
            ./.github/review-bot-setup.sh
          else
            echo "No .github/review-bot-setup.sh found, skipping project setup"
          fi

      - name: Install Claude Code
        run: npm install -g @anthropic-ai/claude-code

      - name: Configure git
        run: |
          git config user.name "github-actions[bot]"
          git config user.email "41898282+github-actions[bot]@users.noreply.github.com"

      - name: Notify in-progress
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          SLACK_WEBHOOK_URL: ${{ secrets.SLACK_WEBHOOK_URL }}
        run: |
          gh api "repos/${{ github.repository }}/pulls/comments/${{ github.event.comment.id }}/reactions" \
            -f content="eyes"

          RUN_URL="${{ github.server_url }}/${{ github.repository }}/actions/runs/${{ github.run_id }}"

          if [ -n "$SLACK_WEBHOOK_URL" ]; then
            curl -s -X POST "$SLACK_WEBHOOK_URL" \
              -H "Content-Type: application/json" \
              -d "$(cat <<EOF
          {"blocks":[{"type":"section","text":{"type":"mrkdwn","text":":hourglass_flowing_sand: *Review bot working*\n*PR:* <${{ github.event.pull_request.html_url }}|${{ github.event.pull_request.title }}>\n*Comment by:* @${{ github.event.comment.user.login }} on \`${{ github.event.comment.path }}:${{ github.event.comment.line }}\`\n<${{ github.event.comment.html_url }}|View comment> · <${RUN_URL}|View logs>"}}]}
          EOF
          )"
          fi

      - name: Run Claude Code
        id: claude
        env:
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
          CLAUDE_STATUS_FILE: /tmp/review-bot-status.json
        run: |
          claude -p "$(cat <<'PROMPT'
          You are a PR review bot.

          ## Context

          A reviewer left this comment on PR #${{ github.event.pull_request.number }}:

          **Comment by @${{ github.event.comment.user.login }}** on file `${{ github.event.comment.path }}` line ${{ github.event.comment.line }}:
          > ${{ github.event.comment.body }}

          Diff context:
          ```
          ${{ github.event.comment.diff_hunk }}
          ```

          ## Triage rules

          Classify the comment as one of:
          - **APPLY**: Small, clear code change suggestion (≤5 net lines changed). You understand exactly what to change and are confident it's correct.
          - **FLAG**: Actionable suggestion but too large (>5 lines), ambiguous, or you're not confident in the fix.
          - **SKIP**: Not actionable — question, praise, discussion, or already addressed.

          Be conservative. If unsure, FLAG rather than APPLY.

          ## If APPLY

          1. Read the relevant file(s) and understand the surrounding code
          2. Make the suggested change
          3. Read `.github/workflows/ci.yml` and run every check/test command listed there, in order. This ensures exact CI parity. Fix any failures (up to 3 attempts per check).
          4. Once all checks pass, commit with message: `address review: <one-line summary>`
          5. Push the commit
          6. Write a JSON file to the path in $CLAUDE_STATUS_FILE: {"action": "APPLIED", "summary": "<what you changed>"}

          ## If FLAG

          Do NOT make any code changes.
          Write a JSON file to $CLAUDE_STATUS_FILE: {"action": "FLAGGED", "summary": "<what the reviewer wants>", "reasoning": "<why you flagged>"}

          ## If SKIP

          Write a JSON file to $CLAUDE_STATUS_FILE: {"action": "SKIPPED", "summary": "<brief reason>"}
          PROMPT
          )" --dangerously-skip-permissions --max-turns 25 --verbose

      - name: Reply to PR comment
        if: always()
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          STATUS_FILE="/tmp/review-bot-status.json"
          if [ ! -f "$STATUS_FILE" ]; then
            echo "No status file found, Claude may have failed"
            gh api "repos/${{ github.repository }}/pulls/${{ github.event.pull_request.number }}/comments/${{ github.event.comment.id }}/reactions" \
              -f content="confused"
            exit 0
          fi

          ACTION=$(python3 -c "import json; print(json.load(open('$STATUS_FILE'))['action'])")
          SUMMARY=$(python3 -c "import json; print(json.load(open('$STATUS_FILE'))['summary'])")

          if [ "$ACTION" = "APPLIED" ]; then
            SHA=$(git rev-parse --short HEAD)
            BODY="Applied this suggestion in $SHA. All CI checks passed locally."
            gh api "repos/${{ github.repository }}/pulls/comments/${{ github.event.comment.id }}/reactions" \
              -f content="rocket"
          elif [ "$ACTION" = "FLAGGED" ]; then
            REASONING=$(python3 -c "import json; print(json.load(open('$STATUS_FILE')).get('reasoning', 'N/A'))")
            BODY="Flagged for manual review.\n\n**Summary:** $SUMMARY\n**Reason:** $REASONING"
          else
            echo "Action is SKIPPED, no reply needed"
            gh api "repos/${{ github.repository }}/pulls/comments/${{ github.event.comment.id }}/reactions" \
              -f content="+1"
            exit 0
          fi

          gh api "repos/${{ github.repository }}/pulls/${{ github.event.pull_request.number }}/comments/${{ github.event.comment.id }}/replies" \
            -f body="$BODY"

      - name: Post-process (Slack + wait for CI + re-request review)
        if: always()
        env:
          SLACK_WEBHOOK_URL: ${{ secrets.SLACK_WEBHOOK_URL }}
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          PR_NUMBER: ${{ github.event.pull_request.number }}
          PR_TITLE: ${{ github.event.pull_request.title }}
          PR_URL: ${{ github.event.pull_request.html_url }}
          PR_HEAD_REF: ${{ github.event.pull_request.head.ref }}
          COMMENT_URL: ${{ github.event.comment.html_url }}
          COMMENT_USER: ${{ github.event.comment.user.login }}
          COMMENT_PATH: ${{ github.event.comment.path }}
          COMMENT_LINE: ${{ github.event.comment.line }}
          REPO: ${{ github.repository }}
          STATUS_FILE: /tmp/review-bot-status.json
        run: python3 .github/scripts/post-process.py
WORKFLOW_EOF
  echo "  ✓ .github/workflows/pr-review-bot.yml"

  # --- post-process.py ---
  cat > "$REPO_DIR/.github/scripts/post-process.py" << 'PYTHON_EOF'
#!/usr/bin/env python3
"""Post-processing for PR Review Bot: Slack notifications, CI wait, re-request review."""

import json
import os
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

CI_WORKFLOW = "ci.yml"
CI_POLL_INTERVAL = 30
CI_TIMEOUT = 1200


def env(name, default=""):
    return os.environ.get(name, default)


SLACK_WEBHOOK_URL = env("SLACK_WEBHOOK_URL")
PR_NUMBER = env("PR_NUMBER")
PR_TITLE = env("PR_TITLE")
PR_URL = env("PR_URL")
PR_HEAD_REF = env("PR_HEAD_REF")
COMMENT_URL = env("COMMENT_URL")
COMMENT_USER = env("COMMENT_USER")
COMMENT_PATH = env("COMMENT_PATH")
COMMENT_LINE = env("COMMENT_LINE")
REPO = env("REPO")
STATUS_FILE = env("STATUS_FILE", "/tmp/review-bot-status.json")


def run(cmd, input_data=None):
    return subprocess.run(cmd, input=input_data, capture_output=True, text=True)


def gh_api(method, endpoint, data=None):
    if data:
        result = run(
            ["gh", "api", "-X", method, endpoint, "--input", "-"], json.dumps(data)
        )
    else:
        result = run(["gh", "api", "-X", method, endpoint])
    if result.returncode != 0:
        print(f"gh api error: {result.stderr}", file=sys.stderr)
        return None
    return json.loads(result.stdout) if result.stdout.strip() else None


def send_slack(text):
    if not SLACK_WEBHOOK_URL:
        print("SLACK_WEBHOOK_URL not set, skipping notification", file=sys.stderr)
        return
    payload = json.dumps(
        {"blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": text}}]}
    ).encode()
    req = urllib.request.Request(
        SLACK_WEBHOOK_URL, data=payload, headers={"Content-Type": "application/json"}
    )
    try:
        urllib.request.urlopen(req, timeout=10)
    except Exception as e:
        print(f"Slack error: {e}", file=sys.stderr)


def read_status():
    p = Path(STATUS_FILE)
    if not p.exists():
        print(f"Status file not found: {STATUS_FILE}")
        return {"action": "UNKNOWN"}
    try:
        return json.loads(p.read_text())
    except json.JSONDecodeError as e:
        print(f"Failed to parse status file: {e}", file=sys.stderr)
        return {"action": "UNKNOWN"}


def wait_for_ci():
    print(f"Waiting for CI ({CI_WORKFLOW}) on branch {PR_HEAD_REF}...")
    time.sleep(10)
    elapsed = 0
    while elapsed < CI_TIMEOUT:
        result = run([
            "gh", "run", "list", "--repo", REPO, "--branch", PR_HEAD_REF,
            "--workflow", CI_WORKFLOW, "--limit", "1",
            "--json", "databaseId,status,conclusion",
        ])
        if result.returncode == 0 and result.stdout.strip():
            runs = json.loads(result.stdout)
            if runs:
                latest = runs[0]
                run_id = latest["databaseId"]
                status = latest["status"]
                conclusion = latest.get("conclusion")
                if status == "completed":
                    if conclusion == "success":
                        print(f"CI run {run_id} passed")
                        return True
                    else:
                        print(f"CI run {run_id} finished: {conclusion}")
                        return False
                else:
                    print(f"CI run {run_id}: {status} (waiting...)")
        time.sleep(CI_POLL_INTERVAL)
        elapsed += CI_POLL_INTERVAL
    print(f"CI timed out after {CI_TIMEOUT}s")
    return False


def all_comments_addressed():
    comments = gh_api("GET", f"/repos/{REPO}/pulls/{PR_NUMBER}/comments") or []
    top_level = [
        c for c in comments
        if not c.get("in_reply_to_id")
        and not c["user"]["login"].endswith("[bot]")
        and c["user"]["login"] != "github-actions"
    ]
    addressed_ids = set()
    for c in comments:
        if c.get("in_reply_to_id") and c["user"]["login"].endswith("[bot]"):
            addressed_ids.add(c["in_reply_to_id"])
    unaddressed = [c for c in top_level if c["id"] not in addressed_ids]
    if unaddressed:
        print(f"{len(unaddressed)} comment(s) still unaddressed")
        return False
    return True


def rerequest_review():
    print(f"All comments addressed and CI passed. Re-requesting review from @{COMMENT_USER}")
    gh_api("POST", f"/repos/{REPO}/pulls/{PR_NUMBER}/requested_reviewers",
           {"reviewers": [COMMENT_USER]})


def main():
    status = read_status()
    action = status.get("action", "UNKNOWN")
    summary = status.get("summary", "")
    reasoning = status.get("reasoning", "")
    print(f"Post-process: action={action}, summary={summary}")

    if action == "FLAGGED":
        line_ref = f"{COMMENT_PATH}:{COMMENT_LINE}" if COMMENT_PATH else "N/A"
        send_slack(
            f":mag: *PR Review needs your attention*\n"
            f"*PR:* <{PR_URL}|{PR_TITLE}>\n"
            f"*Comment by:* @{COMMENT_USER}\n"
            f"*File:* `{line_ref}`\n\n"
            f"*Suggestion:* {summary}\n"
            f"*Why flagged:* {reasoning}\n\n"
            f"<{COMMENT_URL}|View comment>"
        )

    if action == "APPLIED":
        ci_passed = wait_for_ci()
        if ci_passed:
            send_slack(
                f":white_check_mark: *Review suggestion applied and CI passed*\n"
                f"*PR:* <{PR_URL}|{PR_TITLE}>\n"
                f"*Summary:* {summary}\n"
                f"<{COMMENT_URL}|View comment>"
            )
            if all_comments_addressed():
                rerequest_review()
        else:
            send_slack(
                f":x: *CI failed after applying review suggestion*\n"
                f"*PR:* <{PR_URL}|{PR_TITLE}>\n"
                f"*Comment by:* @{COMMENT_USER}\n"
                f"<{COMMENT_URL}|View comment>"
            )
    elif action in ("FLAGGED", "SKIPPED") and all_comments_addressed():
        rerequest_review()


if __name__ == "__main__":
    main()
PYTHON_EOF
  echo "  ✓ .github/scripts/post-process.py"

  # --- Template review-bot-setup.sh (only if it doesn't exist) ---
  if [ ! -f "$REPO_DIR/.github/review-bot-setup.sh" ]; then
    cat > "$REPO_DIR/.github/review-bot-setup.sh" << 'SETUP_EOF'
#!/bin/bash
# Project-specific setup for the PR review bot.
# Install any toolchains or test dependencies your ci.yml needs.
# This runs on ubuntu-latest before Claude Code.
#
# Examples:
#   pip install pytest ruff
#   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
#   source "$HOME/.cargo/env"

set -euo pipefail

echo "No project-specific setup configured. Edit .github/review-bot-setup.sh"
SETUP_EOF
    chmod +x "$REPO_DIR/.github/review-bot-setup.sh"
    echo "  ✓ .github/review-bot-setup.sh (template — edit for your project)"
  else
    echo "  · .github/review-bot-setup.sh already exists, skipping"
  fi

  echo ""
else
  echo "Not in a local repo — skipping file creation."
  echo "You'll need to copy the workflow files manually."
  echo ""
fi

# --- Set secrets ---
echo "━━━ 1/3: Anthropic API Key ━━━"
echo "Get one at: https://console.anthropic.com/settings/keys"
echo ""
gh secret set ANTHROPIC_API_KEY --repo "$REPO"
echo ""
echo "✓ ANTHROPIC_API_KEY set"
echo ""

echo "━━━ 2/3: Slack Webhook URL ━━━"
echo "Create at: https://api.slack.com/apps → your app → Incoming Webhooks"
echo ""
gh secret set SLACK_WEBHOOK_URL --repo "$REPO"
echo ""
echo "✓ SLACK_WEBHOOK_URL set"
echo ""

echo "━━━ 3/3: Personal Access Token (PAT) ━━━"
echo "Create a fine-grained PAT at: https://github.com/settings/tokens?type=beta"
echo "  Repository access: Only select '$REPO'"
echo "  Permissions:"
echo "    Contents:  Read and write"
echo "    Workflows: Read and write"
echo ""
gh secret set PAT_TOKEN --repo "$REPO"
echo ""
echo "✓ PAT_TOKEN set"
echo ""

echo "━━━ Verifying ━━━"
gh secret list --repo "$REPO"
echo ""
echo "╔══════════════════════════════════════════╗"
echo "║  Done! Commit and push the new files,    ║"
echo "║  then leave a review comment on any PR   ║"
echo "║  to test.                                ║"
echo "╚══════════════════════════════════════════╝"
