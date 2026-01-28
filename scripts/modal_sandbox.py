#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "modal==1.2.5",
#     "click>=8.0",
# ]
# ///
"""Modal sandbox management for shotgun.

Unified CLI for creating, executing commands on, and destroying Modal sandboxes.
"""

import json
import os
import sys
from pathlib import Path

import click
import modal


def copy_dir_to_sandbox(sandbox, local_dir: str, remote_dir: str) -> None:
    """Recursively copy a local directory to the sandbox."""
    for root, dirs, files in os.walk(local_dir):
        # Skip hidden dirs and common non-essential dirs
        dirs[:] = [
            d
            for d in dirs
            if not d.startswith(".")
            and d not in ("__pycache__", "node_modules", "target", ".venv", "venv")
        ]

        for fname in files:
            if fname.startswith(".") or fname.endswith(".pyc"):
                continue
            local_path = os.path.join(root, fname)
            rel_path = os.path.relpath(local_path, local_dir)
            remote_path = os.path.join(remote_dir, rel_path)

            # Create parent directory
            remote_parent = os.path.dirname(remote_path)
            if remote_parent and remote_parent != remote_dir:
                try:
                    sandbox.mkdir(remote_parent, parents=True)
                except Exception:
                    pass  # Already exists

            # Copy file
            with open(local_path, "rb") as f:
                content = f.read()
            with sandbox.open(remote_path, "wb") as f:
                f.write(content)


@click.group()
def cli():
    """Modal sandbox management for shotgun."""
    pass


@cli.group()
def create():
    """Create a Modal sandbox."""
    pass


@create.command("default")
def create_default():
    """Create a basic pytest sandbox with examples/tests copied."""
    app = modal.App.lookup("shotgun-sandbox", create_if_missing=True)
    image = modal.Image.debian_slim(python_version="3.11").pip_install("pytest")

    sandbox = modal.Sandbox.create(
        app=app,
        image=image,
        workdir="/app",
        timeout=3600,
    )

    # Copy only the test files we need
    cwd = os.getcwd()
    sandbox.mkdir("/app/examples/tests", parents=True)
    copy_dir_to_sandbox(
        sandbox, os.path.join(cwd, "examples/tests"), "/app/examples/tests"
    )

    print(sandbox.object_id)


@create.command("rust")
def create_rust():
    """Create a Rust sandbox with cargo toolchain."""
    app = modal.App.lookup("shotgun-rust-sandbox", create_if_missing=True)

    image = (
        modal.Image.debian_slim()
        .apt_install("curl", "build-essential")
        .run_commands(
            "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y",
            "echo 'source $HOME/.cargo/env' >> ~/.bashrc",
        )
        .env(
            {
                "PATH": "/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
            }
        )
    )

    sandbox = modal.Sandbox.create(
        app=app,
        image=image,
        workdir="/app",
        timeout=3600,
    )

    # Copy the entire project (source code needed for cargo test)
    cwd = os.getcwd()
    copy_dir_to_sandbox(sandbox, cwd, "/app")

    print(sandbox.object_id)


@create.command("computronium")
@click.option(
    "--gi-path",
    envvar="GI_PATH",
    default="/Users/jacobkirmayer/imbue/generally_intelligent",
    help="Path to generally_intelligent repository",
)
def create_computronium(gi_path: str):
    """Create a computronium test sandbox."""
    print("Creating Modal app...", file=sys.stderr)
    app = modal.App.lookup("shotgun-computronium", create_if_missing=True)

    print("Building image (cached after first run)...", file=sys.stderr)
    image = (
        modal.Image.debian_slim(python_version="3.11")
        .run_commands("echo 'cache-bust-v4'")  # Force rebuild
        .pip_install(
            # Core test deps
            "pytest",
            "pytest-asyncio",
            "pytest-mock",
            "syrupy",
            "inline-snapshot",
            # imbue_core deps (from scanning imports)
            "anyio",
            "attrs",
            "boto3",
            "cachetools",
            "cattrs",
            "diskcache",
            "httpx",
            "loguru",
            "orjson",
            "pathspec",
            "pydantic",
            "pydantic-settings",
            "pyhumps",
            "python-dateutil",
            "python-gitlab",
            "sentry-sdk",
            "tblib",
            "tenacity",
            "toml",
            "traceback-with-variables>=2.2.0",
            "typeid-python",
            "typing-extensions",
            "yasoo",
        )
        # Bake source files into image (cached!)
        # Exclude conftest.py which has heavy dependencies
        .add_local_dir(
            f"{gi_path}/computronium/computronium",
            "/app/computronium",
            ignore=["conftest.py", "*.pyc", "__pycache__"],
        )
        .add_local_dir(
            f"{gi_path}/imbue_core/imbue_core",
            "/app/imbue_core",
            ignore=["conftest.py", "*.pyc", "__pycache__"],
        )
    )

    print("Creating sandbox...", file=sys.stderr)
    sandbox = modal.Sandbox.create(
        app=app,
        image=image,
        workdir="/app",
        timeout=3600,
    )
    print(f"Sandbox ready: {sandbox.object_id}", file=sys.stderr)
    print(sandbox.object_id)


@create.command("sculptor")
@click.option(
    "--gi-path",
    envvar="GI_PATH",
    default=None,
    help="Path to generally_intelligent repository",
)
def create_sculptor(gi_path: str | None):
    """Create a sculptor test sandbox."""
    modal.enable_output()

    # Default to the directory containing this script's grandparent
    if gi_path is None:
        gi_path = str(Path(__file__).parent.parent.parent / "generally_intelligent")

    print(f"Using GI_PATH: {gi_path}", file=sys.stderr)
    print("Creating Modal app...", file=sys.stderr)
    app = modal.App.lookup("shotgun-sculptor", create_if_missing=True)

    print("Building image with dependencies...", file=sys.stderr)
    image = (
        modal.Image.debian_slim(python_version="3.11")
        .run_commands("echo 'cache-bust-v18'")  # Force rebuild
        .apt_install(
            "git",
            "libgit2-dev",  # Required for pygit2
            "curl",
        )
        .pip_install(
            # ===== sculptor dependencies =====
            "alembic>=1.16.1",
            "anthropic>=0.38.0",
            "beautifulsoup4>=4.12.2",
            "coolname>=2.2.0",
            "dockerfile-parse>=2.0.1",
            "email-validator",
            "fastapi",
            "filelock>=3.8.0",
            "humanfriendly>=10.0",
            "json5>=0.9.0",
            "loguru",
            "modal>=1.0.3",
            "psutil>=5.9.0",
            "psycopg[binary]",
            "pydantic-settings",
            "pyjwt[crypto]",
            "requests>=2.28.0",
            "sentry-sdk",
            "splinter>=0.19.0",
            "sqlalchemy",
            "tomlkit>=0.13.0",
            "typeid-python",
            "typer",
            "uvicorn>=0.34.3",
            "watchdog>=6.0.0",
            "websockets>=15.0.1",
            # ===== imbue_core dependencies =====
            "anyio",
            "attrs",
            "boto3>=1.38.27",
            "cachetools",
            "cattrs",
            "diskcache>=5.6.3",
            "grpclib>=0.4.7",
            "httpx",
            "inline-snapshot",
            "pathspec",
            "posthog==5.4.0",
            "prometheus-client>=0.20.0",
            "pydantic>=2.11.4",
            "pygit2>=1.18.0",
            "pylint==3.2.6",
            "pygments>=2.0.0",
            "pyhumps",
            "python-gitlab>=4.5.0",
            "tblib==2.0.0",
            "tenacity>=8.2.2",
            "toml",
            "traceback-with-variables>=2.2.0",
            "yasoo",
            "anthropic~=0.54",
            "tokenizers",
            "openai>=1.79.0",
            "tiktoken",
            "together",
            "groq>=0.18.0",
            "google-genai>=1.26.0",
            # ===== test dependencies =====
            "pytest",
            "pytest-asyncio",
            "pytest-mock",
            "pytest-timeout",
            "syrupy",
            "moto[s3]",
            "boto3-stubs",
            "starlette-context",
            "python-dateutil",
            "orjson",
            "packaging",
            "pytest-xdist>=3.8.0",
        )
        # Bake source files into image (including conftest.py for fixtures)
        .add_local_dir(
            f"{gi_path}/sculptor/sculptor",
            "/app/sculptor",
            ignore=["*.pyc", "__pycache__"],
        )
        .add_local_dir(
            f"{gi_path}/imbue_core/imbue_core",
            "/app/imbue_core",
            ignore=["*.pyc", "__pycache__"],
        )
        # Include root conftest.py for test fixtures
        .add_local_file(f"{gi_path}/sculptor/conftest.py", "/app/conftest.py")
        # Include pyproject.toml for version detection
        .add_local_file(f"{gi_path}/sculptor/pyproject.toml", "/app/pyproject.toml")
    )

    print("Creating sandbox...", file=sys.stderr)
    try:
        sandbox = modal.Sandbox.create(
            app=app,
            image=image,
            workdir="/app",
            timeout=3600,
        )
        print(f"Sandbox ready: {sandbox.object_id}", file=sys.stderr)
        print(sandbox.object_id)
    except Exception as e:
        print(f"Error creating sandbox: {e}", file=sys.stderr)
        raise


@cli.command()
@click.argument("sandbox_id")
def destroy(sandbox_id: str):
    """Terminate a Modal sandbox."""
    sandbox = modal.Sandbox.from_id(sandbox_id)
    sandbox.terminate()
    print(f"Terminated sandbox {sandbox_id}")


@cli.command("exec")
@click.argument("sandbox_id")
@click.argument("command")
def exec_command(sandbox_id: str, command: str):
    """Execute a command on an existing Modal sandbox."""
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


# App and function for the 'run' subcommand
run_app = modal.App("shotgun-test")
run_image = modal.Image.debian_slim(python_version="3.11").pip_install("pytest")


@run_app.function(image=run_image, timeout=600)
def _run_test(cmd: str) -> dict:
    import subprocess

    result = subprocess.run(cmd, shell=True, capture_output=True, text=True)
    # Print output for streaming visibility
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    return {
        "exit_code": result.returncode,
        "stdout": result.stdout,
        "stderr": result.stderr,
    }


@cli.command()
@click.argument("command")
def run(command: str):
    """Run a test command on Modal (ephemeral function execution)."""
    with run_app.run():
        result = _run_test.remote(command)

    # Output JSON for shotgun to parse
    print(json.dumps(result))
    sys.exit(result["exit_code"])


if __name__ == "__main__":
    cli()
