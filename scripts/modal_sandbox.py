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
        # Build default image with cwd baked in
        logger.info("Building default image with cwd baked in...")
        app = modal.App.lookup("offload-sandbox", create_if_missing=True)
        image = (
            modal.Image.debian_slim(python_version="3.11")
            .pip_install("pytest")
            .add_local_dir(".", "/app", copy=True)
        )
        image.build(app)
        # Create temp sandbox to materialize image_id, then terminate
        temp_sandbox = modal.Sandbox.create(app=app, image=image, timeout=10)
        temp_sandbox.terminate()
        sys.stdout.write("%s\n" % image.object_id)
    else:
        # Build from Dockerfile with cwd baked in
        if not os.path.isfile(dockerfile_path):
            logger.error("Error: Dockerfile not found: %s", dockerfile_path)
            sys.exit(1)

        with modal.enable_output():
            app = modal.App.lookup("offload-dockerfile-sandbox", create_if_missing=True)
            logger.info("Building image from %s with cwd baked in...", dockerfile_path)
            image = (
                modal.Image.from_dockerfile(dockerfile_path)
                .add_local_dir(".", "/app", copy=True)
            )
            image.build(app)
            # Create temp sandbox to materialize image_id, then terminate
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
@click.option(
    "--copy-dir",
    "copy_dirs",
    multiple=True,
    help="Copy local dir to sandbox (format: local_path:remote_path)",
)
def create_from_image(image_id: str, copy_dirs: tuple[str, ...] = ()):
    """Create sandbox using existing image_id.

    IMAGE_ID is the Modal image ID to use.
    """
    t0 = time.time()

    # Log received arguments
    logger.debug("[%.2fs] create_from_image called with:", time.time() - t0)
    logger.debug("[%.2fs]   image_id: %s", time.time() - t0, image_id)
    logger.debug("[%.2fs]   copy_dirs: %s", time.time() - t0, copy_dirs)

    app_name = "offload-sandbox"
    app = modal.App.lookup(app_name, create_if_missing=True)

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

    # Copy user-specified directories
    logger.debug(
        "[%.2fs] Processing %d user-specified copy-dir(s)",
        time.time() - t0,
        len(copy_dirs),
    )
    for i, copy_spec in enumerate(copy_dirs):
        logger.info("[%.2fs] copy_dirs[%d]: '%s'", time.time() - t0, i, copy_spec)
        if ":" not in copy_spec:
            logger.warning(
                "Invalid copy-dir format '%s', expected 'local:remote'", copy_spec
            )
            continue
        local_path, remote_path = copy_spec.split(":", 1)
        if not os.path.isdir(local_path):
            logger.warning("Local directory '%s' not found, skipping", local_path)
            continue
        logger.info(
            "[%.2fs] Copying %s to %s...", time.time() - t0, local_path, remote_path
        )
        copy_dir_to_sandbox(sandbox, local_path, remote_path)
        logger.info("[%.2fs] Copy complete", time.time() - t0)

    logger.info("[%.2fs] Sandbox ready: %s", time.time() - t0, sandbox.object_id)
    sys.stdout.write("%s\n" % sandbox.object_id)


if __name__ == "__main__":
    cli()
