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

import hashlib
import json
import os
import sys
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

import click
import modal

# Cache configuration
CACHE_DIR = Path.home() / ".cache" / "offload"
CACHE_FILE = "modal-images.json"


@dataclass
class ImageCacheEntry:
    """Cache entry for a Modal image ID."""

    image_id: str
    dockerfile_hash: str | None
    mtime: float | None
    created_at: str
    sandbox_type: str


def get_cache_path() -> Path:
    """Returns the full path to the cache file, creating the directory if needed."""
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    return CACHE_DIR / CACHE_FILE


def load_cache() -> dict[str, ImageCacheEntry]:
    """Loads the cache from disk, returns empty dict if not found."""
    cache_path = get_cache_path()
    if not cache_path.exists():
        return {}

    try:
        with open(cache_path, "r") as f:
            data = json.load(f)

        # Convert dict entries to ImageCacheEntry objects
        cache = {}
        for key, entry in data.items():
            cache[key] = ImageCacheEntry(**entry)
        return cache
    except (json.JSONDecodeError, OSError, TypeError, KeyError):
        # If cache is corrupted or invalid, return empty dict
        return {}


def save_cache(cache: dict[str, ImageCacheEntry]) -> None:
    """Saves the cache to disk as JSON."""
    cache_path = get_cache_path()

    # Convert ImageCacheEntry objects to dicts
    data = {}
    for key, entry in cache.items():
        data[key] = {
            "image_id": entry.image_id,
            "dockerfile_hash": entry.dockerfile_hash,
            "mtime": entry.mtime,
            "created_at": entry.created_at,
            "sandbox_type": entry.sandbox_type,
        }

    with open(cache_path, "w") as f:
        json.dump(data, f, indent=2)


def compute_file_hash(path: str) -> str:
    """Computes SHA256 hash of a file's contents."""
    sha256 = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(8192), b""):
            sha256.update(chunk)
    return sha256.hexdigest()


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
                except modal.exception.FilesystemExecutionError:
                    pass

            # Copy file
            with open(local_path, "rb") as f:
                content = f.read()
            with sandbox.open(remote_path, "wb") as f:
                f.write(content)


@click.group()
def cli():
    """Modal sandbox management for offload."""
    pass


@cli.group()
def create():
    """Create a Modal sandbox."""
    pass


@create.command("default")
def create_default():
    """Create a basic pytest sandbox with examples/tests copied."""
    app = modal.App.lookup("offload-sandbox", create_if_missing=True)
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


@create.command("dockerfile")
@click.argument("dockerfile_path")
def create_dockerfile(dockerfile_path: str):
    """Create a sandbox from a Dockerfile.

    DOCKERFILE_PATH is the path to the Dockerfile to build from.
    This is slower than using built-in images but allows custom environments.
    """
    # Validate dockerfile exists
    if not os.path.isfile(dockerfile_path):
        print(f"Error: Dockerfile not found: {dockerfile_path}", file=sys.stderr)
        sys.exit(1)

    app = modal.App.lookup("offload-dockerfile-sandbox", create_if_missing=True)
    image = modal.Image.from_dockerfile(dockerfile_path)

    sandbox = modal.Sandbox.create(
        app=app,
        image=image,
        workdir="/app",
        timeout=3600,
    )

    # Copy the project files to the sandbox
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
    app = modal.App.lookup("offload-computronium", create_if_missing=True)

    print("Building image (cached after first run)...", file=sys.stderr)
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
    app = modal.App.lookup("offload-sculptor", create_if_missing=True)

    print("Building image with dependencies...", file=sys.stderr)
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
    except modal.exception.Error as e:
        print(f"Error creating sandbox: {e}", file=sys.stderr)
        raise


@create.command("mngr")
def create_mngr():
    """Create a mngr test sandbox."""
    modal.enable_output()

    mngr_path = os.getcwd()
    print(f"Using MNGR_PATH: {mngr_path}", file=sys.stderr)
    app = modal.App.lookup("offload-mngr", create_if_missing=True)

    print("Building image with dependencies...", file=sys.stderr)
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
    except modal.exception.Error as e:
        print(f"Error creating sandbox: {e}", file=sys.stderr)
        raise


@cli.group("build-image")
def build_image():
    """Eagerly build and cache Modal images."""
    pass


@build_image.command("default")
def build_image_default():
    """Build and cache the default pytest image."""
    print("Building default image...", file=sys.stderr)
    app = modal.App.lookup("offload-sandbox", create_if_missing=True)
    image = modal.Image.debian_slim(python_version="3.11").pip_install("pytest")

    # Eagerly build the image
    image.build(app)
    image_id = image.object_id

    # Cache the image
    cache = load_cache()
    cache["default"] = ImageCacheEntry(
        image_id=image_id,
        dockerfile_hash=None,
        mtime=None,
        created_at=datetime.now().isoformat(),
        sandbox_type="default",
    )
    save_cache(cache)

    print(f"Cached image: {image_id}", file=sys.stderr)
    print(image_id)


@build_image.command("rust")
def build_image_rust():
    """Build and cache the Rust toolchain image."""
    print("Building rust image...", file=sys.stderr)
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

    # Eagerly build the image
    image.build(app)
    image_id = image.object_id

    # Cache the image
    cache = load_cache()
    cache["rust"] = ImageCacheEntry(
        image_id=image_id,
        dockerfile_hash=None,
        mtime=None,
        created_at=datetime.now().isoformat(),
        sandbox_type="rust",
    )
    save_cache(cache)

    print(f"Cached image: {image_id}", file=sys.stderr)
    print(image_id)


@build_image.command("dockerfile")
@click.argument("dockerfile_path")
def build_image_dockerfile(dockerfile_path: str):
    """Build and cache an image from a Dockerfile.

    DOCKERFILE_PATH is the path to the Dockerfile to build from.
    """
    # Validate dockerfile exists
    if not os.path.isfile(dockerfile_path):
        print(f"Error: Dockerfile not found: {dockerfile_path}", file=sys.stderr)
        sys.exit(1)

    print(f"Building dockerfile image from {dockerfile_path}...", file=sys.stderr)
    app = modal.App.lookup("offload-dockerfile-sandbox", create_if_missing=True)
    image = modal.Image.from_dockerfile(dockerfile_path)

    # Compute dockerfile hash
    dockerfile_hash = compute_file_hash(dockerfile_path)

    # Eagerly build the image
    image.build(app)
    image_id = image.object_id

    # Cache the image
    cache = load_cache()
    cache[f"dockerfile:{dockerfile_path}"] = ImageCacheEntry(
        image_id=image_id,
        dockerfile_hash=dockerfile_hash,
        mtime=None,
        created_at=datetime.now().isoformat(),
        sandbox_type="dockerfile",
    )
    save_cache(cache)

    print(f"Cached image: {image_id}", file=sys.stderr)
    print(image_id)


@build_image.command("computronium")
@click.option(
    "--gi-path",
    envvar="GI_PATH",
    default="/Users/jacobkirmayer/imbue/generally_intelligent",
    help="Path to generally_intelligent repository",
)
def build_image_computronium(gi_path: str):
    """Build and cache the computronium test image."""
    print("Building computronium image...", file=sys.stderr)
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

    # Eagerly build the image
    image.build(app)
    image_id = image.object_id

    # Cache the image
    cache = load_cache()
    cache["computronium"] = ImageCacheEntry(
        image_id=image_id,
        dockerfile_hash=None,
        mtime=None,
        created_at=datetime.now().isoformat(),
        sandbox_type="computronium",
    )
    save_cache(cache)

    print(f"Cached image: {image_id}", file=sys.stderr)
    print(image_id)


@build_image.command("sculptor")
@click.option(
    "--gi-path",
    envvar="GI_PATH",
    default=None,
    help="Path to generally_intelligent repository",
)
def build_image_sculptor(gi_path: str | None):
    """Build and cache the sculptor test image."""
    modal.enable_output()

    # Default to the directory containing this script's grandparent
    if gi_path is None:
        gi_path = str(Path(__file__).parent.parent.parent / "generally_intelligent")

    print(f"Using GI_PATH: {gi_path}", file=sys.stderr)
    print("Building sculptor image...", file=sys.stderr)
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

    # Eagerly build the image
    image.build(app)
    image_id = image.object_id

    # Cache the image
    cache = load_cache()
    cache["sculptor"] = ImageCacheEntry(
        image_id=image_id,
        dockerfile_hash=None,
        mtime=None,
        created_at=datetime.now().isoformat(),
        sandbox_type="sculptor",
    )
    save_cache(cache)

    print(f"Cached image: {image_id}", file=sys.stderr)
    print(image_id)


@build_image.command("mngr")
def build_image_mngr():
    """Build and cache the mngr test image."""
    modal.enable_output()

    mngr_path = os.getcwd()
    print(f"Using MNGR_PATH: {mngr_path}", file=sys.stderr)
    print("Building mngr image...", file=sys.stderr)
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

    # Eagerly build the image
    image.build(app)
    image_id = image.object_id

    # Cache the image
    cache = load_cache()
    cache["mngr"] = ImageCacheEntry(
        image_id=image_id,
        dockerfile_hash=None,
        mtime=None,
        created_at=datetime.now().isoformat(),
        sandbox_type="mngr",
    )
    save_cache(cache)

    print(f"Cached image: {image_id}", file=sys.stderr)
    print(image_id)


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


if __name__ == "__main__":
    cli()
