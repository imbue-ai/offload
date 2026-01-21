#!/usr/bin/env python3
"""Create a Modal sandbox and print its ID."""
import os
import modal

app = modal.App.lookup("shotgun-sandbox", create_if_missing=True)
image = modal.Image.debian_slim(python_version="3.11").pip_install("pytest")

def copy_dir_to_sandbox(sandbox, local_dir, remote_dir):
    """Recursively copy a local directory to the sandbox."""
    for root, dirs, files in os.walk(local_dir):
        # Skip hidden dirs and common non-essential dirs
        dirs[:] = [d for d in dirs if not d.startswith('.') and d not in ('__pycache__', 'node_modules', 'target', '.venv', 'venv')]

        for fname in files:
            if fname.startswith('.') or fname.endswith('.pyc'):
                continue
            local_path = os.path.join(root, fname)
            rel_path = os.path.relpath(local_path, local_dir)
            remote_path = os.path.join(remote_dir, rel_path)

            # Create parent directory
            remote_parent = os.path.dirname(remote_path)
            if remote_parent and remote_parent != remote_dir:
                try:
                    sandbox.mkdir(remote_parent, parents=True)
                except:
                    pass  # Already exists

            # Copy file
            with open(local_path, 'rb') as f:
                content = f.read()
            with sandbox.open(remote_path, 'wb') as f:
                f.write(content)

if __name__ == "__main__":
    # Create sandbox with base image (fast)
    sandbox = modal.Sandbox.create(
        app=app,
        image=image,
        workdir="/app",
        timeout=3600,
    )

    # Copy only the test files we need
    cwd = os.getcwd()
    sandbox.mkdir("/app/examples/tests", parents=True)
    copy_dir_to_sandbox(sandbox, os.path.join(cwd, "examples/tests"), "/app/examples/tests")

    print(sandbox.object_id)
