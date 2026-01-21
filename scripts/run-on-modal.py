#!/usr/bin/env python3
"""Run a test command on Modal."""
import sys
import json

import modal

app = modal.App("shotgun-test")
image = modal.Image.debian_slim(python_version="3.11").pip_install("pytest")

@app.function(image=image, timeout=600)
def run_test(cmd: str) -> dict:
    import subprocess
    result = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    # Print output for streaming visibility
    if result.stdout:
        print(result.stdout, end='')
    if result.stderr:
        print(result.stderr, end='', file=sys.stderr)
    return {
        "exit_code": result.returncode,
        "stdout": result.stdout,
        "stderr": result.stderr,
    }

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: run-on-modal.py <command>", file=sys.stderr)
        sys.exit(1)

    test_cmd = sys.argv[1]
    with app.run():
        result = run_test.remote(test_cmd)

    # Output JSON for shotgun to parse
    print(json.dumps(result))
    sys.exit(result["exit_code"])
