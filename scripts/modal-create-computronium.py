#!/usr/bin/env python3
"""Create a Modal sandbox for computronium tests and print its ID."""
import sys
import modal

GI_PATH = "/Users/jacobkirmayer/imbue/generally_intelligent"

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
    .add_local_dir(f"{GI_PATH}/computronium/computronium", "/app/computronium", ignore=["conftest.py", "*.pyc", "__pycache__"])
    .add_local_dir(f"{GI_PATH}/imbue_core/imbue_core", "/app/imbue_core", ignore=["conftest.py", "*.pyc", "__pycache__"])
)

if __name__ == "__main__":
    print("Creating sandbox...", file=sys.stderr)
    sandbox = modal.Sandbox.create(
        app=app,
        image=image,
        workdir="/app",
        timeout=3600,
    )
    print(f"Sandbox ready: {sandbox.object_id}", file=sys.stderr)
    print(sandbox.object_id)
