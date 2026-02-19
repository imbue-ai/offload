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
Uses async Modal APIs for better CPU yielding during waits.
"""

import asyncio
import io
import json
import logging
import os
import shutil
import sys
import tarfile
import time

import click
import modal

logger = logging.getLogger(__name__)
logger.setLevel(logging.DEBUG)
handler = logging.StreamHandler(sys.stderr)
handler.setFormatter(logging.Formatter("%(message)s"))
logger.addHandler(handler)


async def copy_from_sandbox(sandbox, remote_path: str, local_path: str) -> None:
    """Copy a file or directory from the sandbox to local filesystem using tar."""
    logger.info("Downloading %s to %s...", remote_path, local_path)

    # Use tar on the sandbox to create an archive of the remote path
    # This handles both files and directories uniformly
    tar_remote_path = "/tmp/.download_transfer.tar"

    # Create tar archive on sandbox
    # Use -C to change to parent directory and archive just the basename
    # This preserves the directory structure correctly
    remote_parent = os.path.dirname(remote_path.rstrip("/")) or "/"
    remote_basename = os.path.basename(remote_path.rstrip("/"))

    logger.info("Creating tar archive on sandbox...")
    process = await sandbox.exec.aio(
        "tar", "-cf", tar_remote_path, "-C", remote_parent, remote_basename
    )
    await process.wait.aio()
    if process.returncode != 0:
        stderr = process.stderr.read()
        raise click.ClickException(f"Failed to create tar archive on sandbox: {stderr}")

    # Read the tar archive from sandbox
    logger.info("Transferring tar archive from sandbox...")
    with sandbox.open(tar_remote_path, "rb") as f:
        tar_data = f.read()

    logger.info("Received tar archive (%d bytes)", len(tar_data))

    # Clean up tar file on sandbox
    process = await sandbox.exec.aio("rm", "-f", tar_remote_path)
    await process.wait.aio()

    # Extract tar archive locally
    tar_buffer = io.BytesIO(tar_data)

    # Create parent directory if needed
    local_parent = os.path.dirname(local_path.rstrip("/")) or "."
    os.makedirs(local_parent, exist_ok=True)

    logger.info("Extracting tar archive to %s...", local_parent)
    with tarfile.open(fileobj=tar_buffer, mode="r") as tar:
        tar.extractall(path=local_parent)

    # If local_path differs from the extracted name, rename
    extracted_path = os.path.join(local_parent, remote_basename)
    if extracted_path != local_path and os.path.exists(extracted_path):
        if os.path.exists(local_path):
            # Remove existing target to allow rename
            if os.path.isdir(local_path):
                shutil.rmtree(local_path)
            else:
                os.remove(local_path)
        os.rename(extracted_path, local_path)

    logger.info("Download complete: %s -> %s", remote_path, local_path)


@click.group()
def cli():
    """Modal sandbox management for offload."""
    pass


CACHE_FILE = ".offload-image-cache"
DOCKERIGNORE_FILE = ".dockerignore"


def read_dockerignore_patterns() -> list[str]:
    """Read patterns from .dockerignore file."""
    if not os.path.isfile(DOCKERIGNORE_FILE):
        return []
    patterns = []
    with open(DOCKERIGNORE_FILE) as f:
        for line in f:
            line = line.strip()
            # Skip empty lines and comments
            if line and not line.startswith("#"):
                patterns.append(line)
    return patterns


def read_cached_image_id() -> str | None:
    """Read cached image_id from cache file if it exists."""
    if not os.path.isfile(CACHE_FILE):
        return None
    try:
        with open(CACHE_FILE) as f:
            image_id = f.read().strip()
            if image_id.startswith("im-"):
                return image_id
    except Exception:
        pass
    return None


def write_cached_image_id(image_id: str) -> None:
    """Write image_id to cache file."""
    with open(CACHE_FILE, "w") as f:
        f.write(image_id + "\n")


@cli.command("prepare")
@click.argument("dockerfile_path", required=False, default=None)
@click.option("--cached", is_flag=True, help="Use cached BASE image if available")
@click.option(
    "--include-cwd",
    is_flag=True,
    help="Include current directory in the image (added after cache lookup)",
)
@click.option(
    "--copy-dir",
    "copy_dirs",
    multiple=True,
    help="Copy local dir into image (format: local_path:remote_path)",
)
def prepare(
    dockerfile_path: str | None,
    cached: bool,
    include_cwd: bool,
    copy_dirs: tuple[str, ...],
):
    """Prepare a Modal image (build only, no sandbox creation).

    DOCKERFILE_PATH: Optional path to a Dockerfile. If provided, builds from
    that Dockerfile. If omitted, builds the default pytest image.

    The --cached flag caches only the BASE image (Dockerfile build). The --include-cwd
    and --copy-dir options are applied AFTER cache lookup, ensuring fresh source code
    is always used even when the base image is cached.

    Prints the image_id to stdout for use with 'create'.
    """
    # Read ignore patterns from .dockerignore
    ignore_patterns = read_dockerignore_patterns()
    if ignore_patterns:
        logger.debug(
            "Using %d ignore patterns from %s", len(ignore_patterns), DOCKERIGNORE_FILE
        )

    base_image = None
    base_image_id = None

    # Step 1: Get or build the BASE image (without cwd/copy-dirs)
    if cached:
        base_image_id = read_cached_image_id()
        if base_image_id:
            logger.info("Using cached base image_id: %s", base_image_id)

    if dockerfile_path is None:
        # Build default base image
        with modal.enable_output():
            app = modal.App.lookup("offload-sandbox", create_if_missing=True)

            if base_image_id:
                # Load cached base image
                base_image = modal.Image.from_id(base_image_id)
            else:
                # Build fresh base image
                logger.info("Building default base image...")
                base_image = modal.Image.debian_slim(python_version="3.11").pip_install(
                    "pytest"
                )
                base_image.build(app)
                # Materialize to get base image_id for caching
                temp_sandbox = modal.Sandbox.create(
                    app=app, image=base_image, timeout=10
                )
                temp_sandbox.terminate()
                base_image_id = base_image.object_id
                # Cache the base image
                write_cached_image_id(base_image_id)
                logger.info("Cached base image_id to %s", CACHE_FILE)
    else:
        if not os.path.isfile(dockerfile_path):
            logger.error("Error: Dockerfile not found: %s", dockerfile_path)
            sys.exit(1)

        with modal.enable_output():
            app = modal.App.lookup("offload-dockerfile-sandbox", create_if_missing=True)

            if base_image_id:
                # Load cached base image
                base_image = modal.Image.from_id(base_image_id)
            else:
                # Build fresh base image from Dockerfile
                logger.info(
                    "Building base image from %s with context_dir=.", dockerfile_path
                )
                base_image = modal.Image.from_dockerfile(
                    dockerfile_path, context_dir="."
                )
                base_image.build(app)
                # Materialize to get base image_id for caching
                temp_sandbox = modal.Sandbox.create(
                    app=app, image=base_image, timeout=10
                )
                temp_sandbox.terminate()
                base_image_id = base_image.object_id
                # Cache the base image
                write_cached_image_id(base_image_id)
                logger.info("Cached base image_id to %s", CACHE_FILE)

    # Step 2: Add cwd and copy-dirs on top of the base image (always fresh)
    final_image = base_image

    with modal.enable_output():
        if include_cwd:
            logger.info("Adding current directory as /app...")
            final_image = final_image.add_local_dir(
                ".", "/app", copy=True, ignore=ignore_patterns
            )

        # Add user-specified directories
        for copy_spec in copy_dirs:
            if ":" not in copy_spec:
                logger.warning(
                    "Invalid copy-dir format '%s', expected 'local:remote'",
                    copy_spec,
                )
                continue
            local_path, remote_path = copy_spec.split(":", 1)
            if not os.path.isdir(local_path):
                logger.warning("Local directory '%s' not found, skipping", local_path)
                continue
            logger.info("Adding %s -> %s to image", local_path, remote_path)
            final_image = final_image.add_local_dir(
                local_path, remote_path, copy=True, ignore=ignore_patterns
            )

        # Build and materialize the final image if we added anything
        if final_image is not base_image:
            final_image.build(app)
            temp_sandbox = modal.Sandbox.create(app=app, image=final_image, timeout=10)
            temp_sandbox.terminate()
            image_id = final_image.object_id
        else:
            image_id = base_image_id

    sys.stdout.write("%s\n" % image_id)


@cli.command()
@click.argument("sandbox_id")
def destroy(sandbox_id: str):
    """Terminate a Modal sandbox."""

    async def terminate():
        sandbox = modal.Sandbox.from_id(sandbox_id)
        await sandbox.terminate.aio()
        logger.info("Terminated sandbox %s", sandbox_id)

    asyncio.run(terminate())


@cli.command("download")
@click.argument("sandbox_id")
@click.argument("paths", nargs=-1, required=True)
def download(sandbox_id: str, paths: tuple[str, ...]):
    """Download files or directories from a Modal sandbox.

    SANDBOX_ID is the Modal sandbox ID to download from.

    PATHS are one or more path specifications in the format "remote_path:local_path".
    Each specification downloads the remote path to the local path.
    Both files and directories are supported.

    Examples:

        modal_sandbox.py download sb-abc123 "/app/results:/tmp/results"

        modal_sandbox.py download sb-abc123 "/app/out:./out" "/app/logs:./logs"
    """

    async def download_all():
        sandbox = modal.Sandbox.from_id(sandbox_id)

        for path_spec in paths:
            if ":" not in path_spec:
                logger.error(
                    "Invalid path format '%s', expected 'remote_path:local_path'",
                    path_spec,
                )
                sys.exit(1)

            remote_path, local_path = path_spec.split(":", 1)
            if not remote_path:
                logger.error("Empty remote path in '%s'", path_spec)
                sys.exit(1)
            if not local_path:
                logger.error("Empty local path in '%s'", path_spec)
                sys.exit(1)

            try:
                await copy_from_sandbox(sandbox, remote_path, local_path)
            except Exception as e:
                logger.error("Failed to download %s: %s", remote_path, e)
                sys.exit(1)

        logger.info("Download complete")

    asyncio.run(download_all())


@cli.command("exec")
@click.argument("sandbox_id")
@click.argument("command")
def exec_command(sandbox_id: str, command: str):
    """Execute a command on an existing Modal sandbox."""

    async def run_command():
        sandbox = modal.Sandbox.from_id(sandbox_id)

        # Execute command
        process = await sandbox.exec.aio("bash", "-c", command)

        # Collect output
        stdout = process.stdout.read()
        stderr = process.stderr.read()
        await process.wait.aio()
        exit_code = process.returncode

        # Output JSON result
        result = {
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
        }
        print(json.dumps(result))
        sys.exit(exit_code)

    asyncio.run(run_command())


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
@click.option(
    "--env",
    "env_vars",
    multiple=True,
    help="Environment variable (format: KEY=VALUE)",
)
def create_from_image(image_id: str, env_vars: tuple[str, ...] = ()):
    """Create sandbox using existing image_id.

    IMAGE_ID is the Modal image ID to use.
    """

    async def create_sandbox():
        t0 = time.time()

        # Log received arguments
        logger.debug("[%.2fs] create_from_image called with:", time.time() - t0)
        logger.debug("[%.2fs]   image_id: %s", time.time() - t0, image_id)
        logger.debug("[%.2fs]   env_vars, %d total", time.time() - t0, len(env_vars))

        # Parse environment variables
        env_dict = {}
        for env_spec in env_vars:
            if "=" not in env_spec:
                logger.warning("Invalid env format '%s', expected 'KEY=VALUE'", env_spec)
                continue
            key, value = env_spec.split("=", 1)
            env_dict[key] = value

        app_name = "offload-sandbox"
        app = modal.App.lookup(app_name, create_if_missing=True)

        # Load image from ID
        logger.debug("[%.2fs] Loading image %s...", time.time() - t0, image_id)
        image = modal.Image.from_id(image_id)
        logger.debug("[%.2fs] Image loaded", time.time() - t0)

        # Create secrets from env dict if any
        secrets = []
        if env_dict:
            secrets = [modal.Secret.from_dict(env_dict)]

        logger.debug("[%.2fs] Creating sandbox...", time.time() - t0)
        sandbox = await modal.Sandbox.create.aio(
            app=app,
            image=image,
            workdir="/app",
            timeout=3600,
            secrets=secrets,
        )
        logger.debug("[%.2fs] Sandbox created", time.time() - t0)

        logger.info("[%.2fs] Sandbox ready: %s", time.time() - t0, sandbox.object_id)
        sys.stdout.write("%s\n" % sandbox.object_id)

    asyncio.run(create_sandbox())


if __name__ == "__main__":
    cli()
