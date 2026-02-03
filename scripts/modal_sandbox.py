#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "modal==1.2.5",
#     "click>=8.0",
# ]
# ///
"""Modal sandbox management for offload.

Unified CLI for creating, executing commands on, and destroying Modal sandboxes.
"""

import io
import json
import os
import logging
import sys
import tarfile
import time
from pathlib import Path

import click
import modal

logger = logging.getLogger(__name__)
logger.setLevel(logging.DEBUG)
handler = logging.StreamHandler(sys.stderr)
handler.setFormatter(logging.Formatter("%(message)s"))
logger.addHandler(handler)


def copy_dir_to_sandbox(sandbox, local_dir: str, remote_dir: str) -> None:
    """Recursively copy a local directory to the sandbox using tar."""
    logger.info("Creating tar archive from %s...", local_dir)

    # Create tar archive in memory
    tar_buffer = io.BytesIO()

    with tarfile.open(fileobj=tar_buffer, mode="w") as tar:
        for root, dirs, files in os.walk(local_dir):
            # Filter directories in-place
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
                tar.add(local_path, arcname=rel_path)

    tar_buffer.seek(0)
    tar_data = tar_buffer.getvalue()

    logger.info("Transferring tar archive (%d bytes) to sandbox...", len(tar_data))

    # Create remote directory and transfer tar
    sandbox.mkdir(remote_dir, parents=True)
    tar_remote_path = f"{remote_dir}/.transfer.tar"
    with sandbox.open(tar_remote_path, "wb") as f:
        f.write(tar_data)

    logger.info("Extracting tar archive in %s...", remote_dir)

    # Extract on sandbox
    sandbox.exec("tar", "-xf", tar_remote_path, "-C", remote_dir).wait()

    # Clean up tar file
    sandbox.exec("rm", "-f", tar_remote_path).wait()

    logger.info("Tar-based transfer complete")


@click.group()
def cli():
    """Modal sandbox management for offload."""
    pass


@cli.group()
def create_legacy():
    """Create a Modal sandbox (legacy - use prepare + create instead)."""
    pass


@create_legacy.command("default")
def create_default():
    """Create a basic pytest sandbox with examples/tests copied."""
    app = modal.App.lookup("offload-sandbox", create_if_missing=True)

    logger.info("Building image...")
    image = modal.Image.debian_slim(python_version="3.11").pip_install("pytest")
    image.build(app)

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

    sys.stdout.write("%s\n" % sandbox.object_id)


@create_legacy.command("rust")
def create_rust():
    """Create a Rust sandbox with cargo toolchain."""
    app = modal.App.lookup("offload-rust-sandbox", create_if_missing=True)

    logger.info("Building image...")
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
    image.build(app)

    sandbox = modal.Sandbox.create(
        app=app,
        image=image,
        workdir="/app",
        timeout=3600,
    )

    # Copy the entire project (source code needed for cargo test)
    cwd = os.getcwd()
    copy_dir_to_sandbox(sandbox, cwd, "/app")

    sys.stdout.write("%s\n" % sandbox.object_id)


@create_legacy.command("dockerfile")
@click.argument("dockerfile_path")
def create_dockerfile(dockerfile_path: str):
    """Create a sandbox from a Dockerfile.

    DOCKERFILE_PATH is the path to the Dockerfile to build from.
    This is slower than using built-in images but allows custom environments.
    """
    # Validate dockerfile exists
    if not os.path.isfile(dockerfile_path):
        logger.error("Error: Dockerfile not found: %s", dockerfile_path)
        sys.exit(1)

    app = modal.App.lookup("offload-dockerfile-sandbox", create_if_missing=True)

    logger.info("Building image from Dockerfile...")
    image = modal.Image.from_dockerfile(dockerfile_path)
    image.build(app)

    sandbox = modal.Sandbox.create(
        app=app,
        image=image,
        workdir="/app",
        timeout=3600,
    )

    # Copy the project files to the sandbox
    cwd = os.getcwd()
    copy_dir_to_sandbox(sandbox, cwd, "/app")

    sys.stdout.write("%s\n" % sandbox.object_id)


@create_legacy.command("computronium")
@click.option(
    "--gi-path",
    envvar="GI_PATH",
    default="/Users/jacobkirmayer/imbue/generally_intelligent",
    help="Path to generally_intelligent repository",
)
def create_computronium(gi_path: str):
    """Create a computronium test sandbox."""
    logger.info("Creating Modal app...")
    app = modal.App.lookup("offload-computronium", create_if_missing=True)

    logger.info("Building image...")
    image = (
        modal.Image.debian_slim(python_version="3.11")
        .run_commands("echo 'cache-bust-v4'")
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
    image.build(app)

    logger.info("Creating sandbox...")
    sandbox = modal.Sandbox.create(
        app=app,
        image=image,
        workdir="/app",
        timeout=3600,
    )
    logger.info("Sandbox ready: %s", sandbox.object_id)
    sys.stdout.write("%s\n" % sandbox.object_id)


@create_legacy.command("sculptor")
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

    logger.info("Using GI_PATH: %s", gi_path)
    logger.info("Creating Modal app...")
    app = modal.App.lookup("offload-sculptor", create_if_missing=True)

    logger.info("Building image with dependencies...")
    image = (
        modal.Image.debian_slim(python_version="3.11")
        .run_commands("echo 'cache-bust-v18'")
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
    image.build(app)

    logger.info("Creating sandbox...")
    try:
        sandbox = modal.Sandbox.create(
            app=app,
            image=image,
            workdir="/app",
            timeout=3600,
        )
        logger.info("Sandbox ready: %s", sandbox.object_id)
        sys.stdout.write("%s\n" % sandbox.object_id)
    except modal.exception.Error as e:
        logger.error("Error creating sandbox: %s", e)
        raise


@create_legacy.command("mngr")
def create_mngr():
    """Create a mngr test sandbox."""
    modal.enable_output()

    mngr_path = os.getcwd()
    logger.info("Using MNGR_PATH: %s", mngr_path)
    app = modal.App.lookup("offload-mngr", create_if_missing=True)

    logger.info("Building image with dependencies...")
    image = (
        modal.Image.debian_slim(python_version="3.11")
        .apt_install(
            "git",
            "tmux",
            "rsync",
        )
        # Initialize /app as a git repo so ratchet tests work
        .run_commands(
            "git config --global user.email 'test@example.com'",
            "git config --global user.name 'Test User'",
            "git config --global init.defaultBranch main",
        )
        .pip_install(
            # ===== imbue-common dependencies =====
            "click>=8.0",
            "cowsay-python>=1.0.2",
            "deal>=4.24",
            "httpx>=0.27",
            "inline-snapshot>=0.13",
            "loguru>=0.7",
            "pydantic>=2.0",
            "tenacity>=8.0",
            # ===== mngr dependencies =====
            "cel-python>=0.1.5",
            "click-option-group>=0.5.6",
            "coolname>=2.2.0,<3.0.0",  # Pin to 2.x for compatibility
            "cryptography>=42.0",
            "dockerfile-parse>=2.0.0",
            "modal>=0.67",
            "psutil>=5.9",
            "pyinfra>=3.0",
            "pluggy>=1.5.0",
            "tabulate>=0.9.0",
            "tomlkit>=0.12.0",
            "urwid>=2.2.0",
            # ===== concurrency_group dependencies =====
            "anyio>=4.4",
            # ===== test dependencies =====
            "pytest>=7.0",
            "pytest-asyncio",
            "pytest-mock",
            "pytest-timeout>=2.3.0",
            "pytest-cov>=7.0.0",
            "pytest-xdist>=3.8.0",
            "coverage>=7.0",
            # ===== dev tools for ratchet tests =====
            "ruff>=0.12.0",
            "ty>=0.0.8",
            "uv",
            # ===== Additional deps for type checking apps/ =====
            "fastapi",
            "uvicorn",
            "flask",
        )
        # Set PYTHONPATH and other env vars
        .env(
            {
                "PYTHONPATH": "/app/libs/imbue_common:/app/libs/mngr:/app/libs/mngr_opencode:/app/libs/concurrency_group:/app/libs/flexmux:/app/apps/claude_web_view:/app/apps/sculptor_desktop:/app/apps/sculptor_web",
                "EDITOR": "cat",  # Simple editor for tests that check --edit-message flag validation
                "VISUAL": "cat",
                # Unset HISTFILE so test_unset_vars_applied_during_agent_start passes
                # (the test expects HISTFILE to be unset, but debian bash sets it by default)
                "HISTFILE": "",
            }
        )
        # Mirror the exact source structure so test paths match
        # Using copy=True so we can run git init after adding files
        .add_local_dir(
            f"{mngr_path}/libs",
            "/app/libs",
            ignore=["*.pyc", "__pycache__", ".venv", "venv", "node_modules"],
            copy=True,
        )
        .add_local_dir(
            f"{mngr_path}/apps",
            "/app/apps",
            ignore=["*.pyc", "__pycache__", ".venv", "venv", "node_modules"],
            copy=True,
        )
        # Include root conftest.py for test fixtures
        .add_local_file(f"{mngr_path}/conftest.py", "/app/conftest.py", copy=True)
        # Include pyproject.toml for pytest configuration
        .add_local_file(f"{mngr_path}/pyproject.toml", "/app/pyproject.toml", copy=True)
        # Initialize git repo after adding files (required for ratchet tests)
        .run_commands(
            "cd /app && git init && git add -A && git commit -m 'Initial commit for tests'"
        )
        # Install local packages so entry points work (required for opencode plugin)
        # Also install apps so type checker can find all dependencies
        .run_commands(
            "pip install -e /app/libs/imbue_common",
            "pip install -e /app/libs/mngr",
            "pip install -e /app/libs/mngr_opencode",
            "pip install -e /app/libs/concurrency_group",
            "pip install -e /app/libs/flexmux",
            "pip install -e /app/apps/claude_web_view",
            "pip install -e /app/apps/sculptor_desktop || true",
            "pip install -e /app/apps/sculptor_web || true",
        )
        # Run uv sync to create proper venv for type checker tests
        .run_commands("cd /app && uv sync --all-packages")
    )
    image.build(app)

    logger.info("Creating sandbox...")
    try:
        sandbox = modal.Sandbox.create(
            app=app,
            image=image,
            workdir="/app",
            timeout=3600,
        )
        logger.info("Sandbox ready: %s", sandbox.object_id)
        sys.stdout.write("%s\n" % sandbox.object_id)
    except modal.exception.Error as e:
        logger.error("Error creating sandbox: %s", e)
        raise


@cli.group()
def build():
    """Build Modal images (called by Rust for cache management)."""
    pass


@cli.command("prepare")
@click.argument("dockerfile_path", required=False, default=None)
def prepare(dockerfile_path: str | None):
    """Prepare a Modal image (build only, no sandbox creation).

    DOCKERFILE_PATH: Optional path to a Dockerfile. If provided, builds from
    that Dockerfile. If omitted, builds the default pytest image.

    Prints the image_id to stdout for use with 'create'.
    """
    # NOTE(Danver): App name here should be injectable from the Config.
    if dockerfile_path is None:
        # Build default image
        logger.info("Building default image...")
        app = modal.App.lookup("offload-sandbox", create_if_missing=True)
        image = modal.Image.debian_slim(python_version="3.11").pip_install("pytest")
        image.build(app)
        # Create temp sandbox to materialize image_id, then terminate
        temp_sandbox = modal.Sandbox.create(app=app, image=image, timeout=10)
        temp_sandbox.terminate()
        sys.stdout.write("%s\n" % image.object_id)
    else:
        # Build from Dockerfile
        if not os.path.isfile(dockerfile_path):
            logger.error("Error: Dockerfile not found: %s", dockerfile_path)
            sys.exit(1)

        with modal.enable_output():
            app = modal.App.lookup("offload-dockerfile-sandbox", create_if_missing=True)
            image = modal.Image.from_dockerfile(dockerfile_path)
            logger.info("Building image from %s...", dockerfile_path)
            image.build(app)
            # Create temp sandbox to materialize image_id, then terminate
            temp_sandbox = modal.Sandbox.create(app=app, image=image, timeout=10)
            temp_sandbox.terminate()

        sys.stdout.write("%s\n" % image.object_id)


@build.command("default")
def build_default():
    """Build default pytest image, print image_id."""
    logger.info("Building default image...")
    app = modal.App.lookup("offload-sandbox", create_if_missing=True)
    image = modal.Image.debian_slim(python_version="3.11").pip_install("pytest")
    image.build(app)
    temp_sandbox = modal.Sandbox.create(app=app, image=image, timeout=10)
    temp_sandbox.terminate()
    sys.stdout.write("%s\n" % image.object_id)


@build.command("rust")
def build_rust():
    """Build Rust toolchain image, print image_id."""
    logger.info("Building rust image...")
    app = modal.App.lookup("offload-rust-sandbox", create_if_missing=True)
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
    image.build(app)
    temp_sandbox = modal.Sandbox.create(app=app, image=image, timeout=10)
    temp_sandbox.terminate()
    sys.stdout.write("%s\n" % image.object_id)


@build.command("dockerfile")
@click.argument("dockerfile_path")
def build_dockerfile(dockerfile_path: str):
    """Build image from Dockerfile, print image_id.

    DOCKERFILE_PATH is the path to the Dockerfile to build from.
    """
    modal.enable_output()

    # Validate dockerfile exists
    if not os.path.isfile(dockerfile_path):
        logger.error("Error: Dockerfile not found: %s", dockerfile_path)
        sys.exit(1)

    logger.info("Building dockerfile image from %s...", dockerfile_path)
    app = modal.App.lookup("offload-dockerfile-sandbox", create_if_missing=True)
    image = modal.Image.from_dockerfile(dockerfile_path)
    image.build(app)
    temp_sandbox = modal.Sandbox.create(app=app, image=image, timeout=10)
    temp_sandbox.terminate()
    sys.stdout.write("%s\n" % image.object_id)


@build.command("preset")
@click.argument("preset_name")
def build_preset(preset_name: str):
    """Build preset image, print image_id.

    PRESET_NAME can be: computronium, sculptor, mngr
    """
    if preset_name == "computronium":
        _build_computronium()
    elif preset_name == "sculptor":
        _build_sculptor()
    elif preset_name == "mngr":
        _build_mngr()
    else:
        logger.error("Error: Unknown preset: %s", preset_name)
        sys.exit(1)


def _build_computronium():
    """Build computronium image."""
    gi_path = os.environ.get(
        "GI_PATH", "/Users/jacobkirmayer/imbue/generally_intelligent"
    )
    logger.info("Building computronium image...")
    app = modal.App.lookup("offload-computronium", create_if_missing=True)

    image = (
        modal.Image.debian_slim(python_version="3.11")
        .run_commands("echo 'cache-bust-v4'")
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
    image.build(app)
    temp_sandbox = modal.Sandbox.create(app=app, image=image, timeout=10)
    temp_sandbox.terminate()
    sys.stdout.write("%s\n" % image.object_id)


def _build_sculptor():
    """Build sculptor image."""
    modal.enable_output()
    gi_path = os.environ.get("GI_PATH")
    if gi_path is None:
        gi_path = str(Path(__file__).parent.parent.parent / "generally_intelligent")

    logger.info("Using GI_PATH: %s", gi_path)
    logger.info("Building sculptor image...")
    app = modal.App.lookup("offload-sculptor", create_if_missing=True)

    image = (
        modal.Image.debian_slim(python_version="3.11")
        .run_commands("echo 'cache-bust-v18'")
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
    image.build(app)
    temp_sandbox = modal.Sandbox.create(app=app, image=image, timeout=10)
    temp_sandbox.terminate()
    sys.stdout.write("%s\n" % image.object_id)


def _build_mngr():
    """Build mngr image."""
    modal.enable_output()
    mngr_path = os.getcwd()
    logger.info("Using MNGR_PATH: %s", mngr_path)
    logger.info("Building mngr image...")
    app = modal.App.lookup("offload-mngr", create_if_missing=True)

    image = (
        modal.Image.debian_slim(python_version="3.11")
        .apt_install(
            "git",
            "tmux",
            "rsync",
        )
        # Initialize /app as a git repo so ratchet tests work
        .run_commands(
            "git config --global user.email 'test@example.com'",
            "git config --global user.name 'Test User'",
            "git config --global init.defaultBranch main",
        )
        .pip_install(
            # ===== imbue-common dependencies =====
            "click>=8.0",
            "cowsay-python>=1.0.2",
            "deal>=4.24",
            "httpx>=0.27",
            "inline-snapshot>=0.13",
            "loguru>=0.7",
            "pydantic>=2.0",
            "tenacity>=8.0",
            # ===== mngr dependencies =====
            "cel-python>=0.1.5",
            "click-option-group>=0.5.6",
            "coolname>=2.2.0,<3.0.0",  # Pin to 2.x for compatibility
            "cryptography>=42.0",
            "dockerfile-parse>=2.0.0",
            "modal>=0.67",
            "psutil>=5.9",
            "pyinfra>=3.0",
            "pluggy>=1.5.0",
            "tabulate>=0.9.0",
            "tomlkit>=0.12.0",
            "urwid>=2.2.0",
            # ===== concurrency_group dependencies =====
            "anyio>=4.4",
            # ===== test dependencies =====
            "pytest>=7.0",
            "pytest-asyncio",
            "pytest-mock",
            "pytest-timeout>=2.3.0",
            "pytest-cov>=7.0.0",
            "pytest-xdist>=3.8.0",
            "coverage>=7.0",
            # ===== dev tools for ratchet tests =====
            "ruff>=0.12.0",
            "ty>=0.0.8",
            "uv",
            # ===== Additional deps for type checking apps/ =====
            "fastapi",
            "uvicorn",
            "flask",
        )
        # Set PYTHONPATH and other env vars
        .env(
            {
                "PYTHONPATH": "/app/libs/imbue_common:/app/libs/mngr:/app/libs/mngr_opencode:/app/libs/concurrency_group:/app/libs/flexmux:/app/apps/claude_web_view:/app/apps/sculptor_desktop:/app/apps/sculptor_web",
                "EDITOR": "cat",  # Simple editor for tests that check --edit-message flag validation
                "VISUAL": "cat",
                # Unset HISTFILE so test_unset_vars_applied_during_agent_start passes
                # (the test expects HISTFILE to be unset, but debian bash sets it by default)
                "HISTFILE": "",
            }
        )
        # Mirror the exact source structure so test paths match
        # Using copy=True so we can run git init after adding files
        .add_local_dir(
            f"{mngr_path}/libs",
            "/app/libs",
            ignore=["*.pyc", "__pycache__", ".venv", "venv", "node_modules"],
            copy=True,
        )
        .add_local_dir(
            f"{mngr_path}/apps",
            "/app/apps",
            ignore=["*.pyc", "__pycache__", ".venv", "venv", "node_modules"],
            copy=True,
        )
        # Include root conftest.py for test fixtures
        .add_local_file(f"{mngr_path}/conftest.py", "/app/conftest.py", copy=True)
        # Include pyproject.toml for pytest configuration
        .add_local_file(f"{mngr_path}/pyproject.toml", "/app/pyproject.toml", copy=True)
        # Initialize git repo after adding files (required for ratchet tests)
        .run_commands(
            "cd /app && git init && git add -A && git commit -m 'Initial commit for tests'"
        )
        # Install local packages so entry points work (required for opencode plugin)
        # Also install apps so type checker can find all dependencies
        .run_commands(
            "pip install -e /app/libs/imbue_common",
            "pip install -e /app/libs/mngr",
            "pip install -e /app/libs/mngr_opencode",
            "pip install -e /app/libs/concurrency_group",
            "pip install -e /app/libs/flexmux",
            "pip install -e /app/apps/claude_web_view",
            "pip install -e /app/apps/sculptor_desktop || true",
            "pip install -e /app/apps/sculptor_web || true",
        )
        # Run uv sync to create proper venv for type checker tests
        .run_commands("cd /app && uv sync --all-packages")
    )
    image.build(app)
    temp_sandbox = modal.Sandbox.create(app=app, image=image, timeout=10)
    temp_sandbox.terminate()
    sys.stdout.write("%s\n" % image.object_id)


@cli.command()
@click.argument("sandbox_id")
def destroy(sandbox_id: str):
    """Terminate a Modal sandbox."""
    sandbox = modal.Sandbox.from_id(sandbox_id)
    sandbox.terminate()
    logger.info("Terminated sandbox %s", sandbox_id)


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
run_app = modal.App("offload-test")
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

    # Output JSON for offload to parse
    print(json.dumps(result))
    sys.exit(result["exit_code"])


@cli.command("create")
@click.argument("image_id")
@click.argument("sandbox_type", default="default", required=False)
@click.option(
    "--copy-dir",
    "copy_dirs",
    multiple=True,
    help="Copy local dir to sandbox (format: local_path:remote_path)",
)
def create_from_image(
    image_id: str, sandbox_type: str = "default", copy_dirs: tuple[str, ...] = ()
):
    """Create sandbox using existing image_id.

    IMAGE_ID is the Modal image ID to use.
    SANDBOX_TYPE is the type of sandbox (default, rust, dockerfile, etc.). Defaults to 'default'.
    """
    t0 = time.time()

    # Log received arguments
    logger.debug("[%.2fs] create_from_image called with:", time.time() - t0)
    logger.debug("[%.2fs]   image_id: %s", time.time() - t0, image_id)
    logger.debug("[%.2fs]   sandbox_type: %s", time.time() - t0, sandbox_type)
    logger.debug("[%.2fs]   copy_dirs: %s", time.time() - t0, copy_dirs)

    # Map sandbox types to appropriate app names and setup logic
    app_name_map = {
        "default": "offload-sandbox",
        "rust": "offload-rust-sandbox",
        "dockerfile": "offload-dockerfile-sandbox",
        "computronium": "offload-computronium",
        "sculptor": "offload-sculptor",
        "mngr": "offload-mngr",
    }

    app_name = app_name_map.get(sandbox_type, "offload-sandbox")
    logger.debug("[%.2fs] Looking up app %s...", time.time() - t0, app_name)
    app = modal.App.lookup(app_name, create_if_missing=True)
    logger.debug("[%.2fs] App lookup complete", time.time() - t0)

    # Load image from ID
    logger.debug("[%.2fs] Loading image %s...", time.time() - t0, image_id)
    image = modal.Image.from_id(image_id)
    logger.debug("[%.2fs] Image loaded", time.time() - t0)

    logger.debug("[%.2fs] Creating sandbox...", time.time() - t0)
    sandbox = modal.Sandbox.create(
        app=app,
        image=image,
        workdir="/app",
        timeout=3600,
    )
    logger.debug("[%.2fs] Sandbox created", time.time() - t0)

    # Copy files based on sandbox type
    cwd = os.getcwd()
    if sandbox_type == "default":
        logger.debug("[%.2fs] Copying files for default sandbox...", time.time() - t0)
        sandbox.mkdir("/app/examples/tests", parents=True)
        copy_dir_to_sandbox(
            sandbox, os.path.join(cwd, "examples/tests"), "/app/examples/tests"
        )
        logger.debug("[%.2fs] File copy complete", time.time() - t0)
    elif sandbox_type in ("rust", "dockerfile"):
        logger.debug(
            "[%.2fs] Copying files for %s sandbox...", time.time() - t0, sandbox_type
        )
        copy_dir_to_sandbox(sandbox, cwd, "/app")
        logger.debug("[%.2fs] File copy complete", time.time() - t0)

    # Copy user-specified directories
    logger.debug(
        "[%.2fs] Processing %d user-specified copy-dir(s)",
        time.time() - t0,
        len(copy_dirs),
    )
    for i, copy_spec in enumerate(copy_dirs):
        logger.debug("[%.2fs] copy_dirs[%d]: '%s'", time.time() - t0, i, copy_spec)
        if ":" not in copy_spec:
            logger.warning(
                "Invalid copy-dir format '%s', expected 'local:remote'", copy_spec
            )
            continue
        local_path, remote_path = copy_spec.split(":", 1)
        if not os.path.isdir(local_path):
            logger.warning("Local directory '%s' not found, skipping", local_path)
            continue
        logger.debug(
            "[%.2fs] Copying %s to %s...", time.time() - t0, local_path, remote_path
        )
        copy_dir_to_sandbox(sandbox, local_path, remote_path)
        logger.debug("[%.2fs] Copy complete", time.time() - t0)

    logger.info("[%.2fs] Sandbox ready: %s", time.time() - t0, sandbox.object_id)
    sys.stdout.write("%s\n" % sandbox.object_id)


if __name__ == "__main__":
    cli()
