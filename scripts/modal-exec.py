#!/usr/bin/env python3
"""Execute a command on an existing Modal sandbox."""
import sys
import json
import modal

if __name__ == "__main__":
    if len(sys.argv) < 3:
        print("Usage: modal-exec.py <sandbox_id> <command>", file=sys.stderr)
        sys.exit(1)

    sandbox_id = sys.argv[1]
    command = sys.argv[2]

    # Reconnect to existing sandbox
    sandbox = modal.Sandbox.from_id(sandbox_id)

    # Execute command
    process = sandbox.exec("bash", "-c", command)

    # Collect output
    stdout = process.stdout.read()
    stderr = process.stderr.read()
    process.wait()
    exit_code = process.returncode

    # Output JSON result
    result = {
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
    }
    print(json.dumps(result))
    sys.exit(exit_code)
