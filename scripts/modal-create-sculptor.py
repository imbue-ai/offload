#!/usr/bin/env python3
"""Create a Modal sandbox for sculptor tests and print its ID."""
import os
import sys
from pathlib import Path
import modal
modal.enable_output()

# Default to the directory containing this script's grandparent (shotgun-rs -> generally_intelligent)
# Can be overridden via GI_PATH environment variable
GI_PATH = os.environ.get("GI_PATH", str(Path(__file__).parent.parent.parent / "generally_intelligent"))

print(f"Using GI_PATH: {GI_PATH}", file=sys.stderr)
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
    .add_local_dir(f"{GI_PATH}/sculptor/sculptor", "/app/sculptor", ignore=["*.pyc", "__pycache__"])
    .add_local_dir(f"{GI_PATH}/imbue_core/imbue_core", "/app/imbue_core", ignore=["*.pyc", "__pycache__"])
    # Include root conftest.py for test fixtures
    .add_local_file(f"{GI_PATH}/sculptor/conftest.py", "/app/conftest.py")
    # Include pyproject.toml for version detection
    .add_local_file(f"{GI_PATH}/sculptor/pyproject.toml", "/app/pyproject.toml")
)

if __name__ == "__main__":
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
