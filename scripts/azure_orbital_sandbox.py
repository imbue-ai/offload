#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "microsoftazurespacefx>=1.0.0",
#     "click>=8.0",
# ]
# ///
"""Azure Orbital Space SDK sandbox management for Offload.

Unified CLI for creating, executing commands on, and destroying Azure Orbital
sandboxes backed by Kubernetes pods with Dapr.

Note: This script uses placeholder implementations for the Azure Orbital Space SDK
calls. The actual SDK integration would require access to the microsoftazurespacefx
package and proper Azure Orbital environment configuration.
"""

import json
import logging
import os
import sys
import time
import uuid

import click

logger = logging.getLogger(__name__)
logger.setLevel(logging.DEBUG)
handler = logging.StreamHandler(sys.stderr)
handler.setFormatter(logging.Formatter("%(message)s"))
logger.addHandler(handler)


CACHE_FILE = ".offload-azure-orbital-image-cache"


def read_cached_image_id() -> str | None:
    """Read cached image_id from cache file if it exists."""
    if not os.path.isfile(CACHE_FILE):
        return None
    if not os.access(CACHE_FILE, os.R_OK):
        return None
    with open(CACHE_FILE) as f:
        image_id = f.read().strip()
        if image_id.startswith("orbital-img-"):
            return image_id
    return None


def write_cached_image_id(image_id: str) -> None:
    """Write image_id to cache file."""
    with open(CACHE_FILE, "w") as f:
        f.write(image_id + "\n")


def clear_image_cache() -> None:
    """Clear the cached image ID file."""
    if os.path.isfile(CACHE_FILE):
        os.remove(CACHE_FILE)
        logger.info("Cleared cached image from %s", CACHE_FILE)


def generate_image_id() -> str:
    """Generate a unique image ID for Azure Orbital."""
    return f"orbital-img-{uuid.uuid4().hex[:16]}"


def generate_sandbox_id() -> str:
    """Generate a unique sandbox/pod ID for Azure Orbital."""
    return f"orbital-pod-{uuid.uuid4().hex[:16]}"


@click.group()
def cli():
    """Azure Orbital Space SDK sandbox management for Offload."""
    pass


@cli.command("prepare")
@click.argument("dockerfile_path", required=False, default=None)
@click.option("--cached", is_flag=True, help="Use cached image if available")
@click.option(
    "--include-cwd",
    is_flag=True,
    help="Include current directory in the image",
)
@click.option(
    "--copy-dir",
    "copy_dirs",
    multiple=True,
    help="Copy local dir into image (format: local_path:remote_path)",
)
@click.option(
    "--sandbox-init-cmd",
    default=None,
    help="Command to run during image build after cwd/copy-dirs are applied",
)
def prepare(
    dockerfile_path: str | None,
    cached: bool,
    include_cwd: bool,
    copy_dirs: tuple[str, ...],
    sandbox_init_cmd: str | None,
):
    """Prepare a container image for Azure Orbital Space SDK.

    DOCKERFILE_PATH: Optional path to a Dockerfile. If provided, builds from
    that Dockerfile. If omitted, uses a default base image.

    The --cached flag uses a cached image if available. The --include-cwd
    and --copy-dir options specify content to include in the image.

    Prints the image_id to stdout for use with 'create'.
    """
    t0 = time.time()

    # Check for cached image if requested
    if cached:
        cached_id = read_cached_image_id()
        if cached_id:
            logger.info("Found cached image_id: %s", cached_id)
            # Note: In production, verify the cached image still exists in Azure Container Registry
            # using microsoftazurespacefx SDK
            sys.stdout.write("%s\n" % cached_id)
            return

    # Validate dockerfile if provided
    if dockerfile_path is not None and not os.path.isfile(dockerfile_path):
        logger.error("Error: Dockerfile not found: %s", dockerfile_path)
        sys.exit(1)

    logger.info("[%.2fs] Preparing Azure Orbital image...", time.time() - t0)

    # Implementation note: Actual Azure Orbital Space SDK implementation would:
    # 1. Build container image from Dockerfile using Azure Container Registry
    # 2. Push image to ACR for use by Azure Orbital pods
    # 3. Return the ACR image reference
    #
    # Example pseudocode:
    # from microsoftazurespacefx import ImageBuilder
    # builder = ImageBuilder()
    # if dockerfile_path:
    #     image = builder.build_from_dockerfile(dockerfile_path, context_dir=".")
    # else:
    #     image = builder.get_default_image()
    # if include_cwd:
    #     image.add_directory(".", "/app")
    # for copy_spec in copy_dirs:
    #     local_path, remote_path = copy_spec.split(":", 1)
    #     image.add_directory(local_path, remote_path)
    # if sandbox_init_cmd:
    #     image.run_command(sandbox_init_cmd)
    # image_id = image.push()

    if dockerfile_path:
        logger.info("[%.2fs] Building from Dockerfile: %s", time.time() - t0, dockerfile_path)
    else:
        logger.info("[%.2fs] Using default base image", time.time() - t0)

    if include_cwd:
        logger.info("[%.2fs] Including current directory as /app", time.time() - t0)

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
        logger.info("[%.2fs] Adding %s -> %s to image", time.time() - t0, local_path, remote_path)

    if sandbox_init_cmd:
        logger.info("[%.2fs] Will run sandbox_init_cmd: %s", time.time() - t0, sandbox_init_cmd)

    # Generate a mock image ID (in production, this would come from ACR)
    image_id = generate_image_id()
    logger.info("[%.2fs] Image prepared: %s", time.time() - t0, image_id)

    # Cache the image ID
    write_cached_image_id(image_id)
    logger.info("[%.2fs] Cached image_id to %s", time.time() - t0, CACHE_FILE)

    sys.stdout.write("%s\n" % image_id)


@cli.command("create")
@click.argument("image_id")
@click.option(
    "--copy-dir",
    "copy_dirs",
    multiple=True,
    help="Copy local dir to sandbox (format: local_path:remote_path)",
)
@click.option(
    "--env",
    "env_vars",
    multiple=True,
    help="Environment variable (format: KEY=VALUE)",
)
@click.option(
    "--cpu",
    type=float,
    default=None,
    help="CPU cores per sandbox",
)
def create(
    image_id: str,
    copy_dirs: tuple[str, ...] = (),
    env_vars: tuple[str, ...] = (),
    cpu: float | None = None,
):
    """Create a sandbox/pod using an existing image_id.

    IMAGE_ID is the Azure Orbital image ID to use.

    Prints the sandbox_id to stdout for use with 'exec' and 'destroy'.
    """
    t0 = time.time()

    logger.debug("[%.2fs] create called with:", time.time() - t0)
    logger.debug("[%.2fs]   image_id: %s", time.time() - t0, image_id)
    logger.debug("[%.2fs]   copy_dirs: %s", time.time() - t0, copy_dirs)
    logger.debug("[%.2fs]   env_vars, %d total", time.time() - t0, len(env_vars))
    logger.debug("[%.2fs]   cpu: %s", time.time() - t0, cpu)

    # Parse environment variables
    env_dict = {}
    for env_spec in env_vars:
        if "=" not in env_spec:
            logger.warning("Invalid env format '%s', expected 'KEY=VALUE'", env_spec)
            continue
        key, value = env_spec.split("=", 1)
        env_dict[key] = value

    # Implementation note: Actual Azure Orbital Space SDK implementation would:
    # 1. Create a Kubernetes pod using the Azure Orbital SDK
    # 2. Configure Dapr sidecar for inter-service communication
    # 3. Set up environment variables and resource limits
    # 4. Wait for pod to be ready
    #
    # Example pseudocode:
    # from microsoftazurespacefx import PodManager
    # manager = PodManager()
    # pod_config = {
    #     "image": image_id,
    #     "env": env_dict,
    #     "resources": {"cpu": cpu} if cpu else {},
    #     "dapr": {"enabled": True},
    # }
    # pod = manager.create_pod(pod_config)
    # pod.wait_until_ready()
    # sandbox_id = pod.id

    logger.info("[%.2fs] Creating Azure Orbital pod with image %s...", time.time() - t0, image_id)

    if env_dict:
        logger.info("[%.2fs] Environment variables: %d", time.time() - t0, len(env_dict))

    if cpu is not None:
        logger.info("[%.2fs] CPU cores: %.2f", time.time() - t0, cpu)

    # Process copy-dirs
    for copy_spec in copy_dirs:
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
            "[%.2fs] Would copy %s to %s in pod", time.time() - t0, local_path, remote_path
        )

    # Generate a mock sandbox ID (in production, this would come from Kubernetes)
    sandbox_id = generate_sandbox_id()
    logger.info("[%.2fs] Pod created: %s", time.time() - t0, sandbox_id)

    sys.stdout.write("%s\n" % sandbox_id)


@cli.command("exec")
@click.argument("sandbox_id")
@click.argument("command")
def exec_command(sandbox_id: str, command: str):
    """Execute a command in an Azure Orbital sandbox/pod.

    SANDBOX_ID is the pod ID to execute in.
    COMMAND is the shell command to run.

    Outputs JSON: {"exit_code": N, "stdout": "...", "stderr": "..."}
    """
    t0 = time.time()

    logger.info("[%.2fs] Executing command in pod %s", time.time() - t0, sandbox_id)
    logger.debug("[%.2fs] Command: %s", time.time() - t0, command)

    # Implementation note: Actual Azure Orbital Space SDK implementation would:
    # 1. Connect to the Kubernetes pod via kubectl exec or SDK
    # 2. Execute the command using bash -c
    # 3. Capture stdout, stderr, and exit code
    # 4. Return results as JSON
    #
    # Example pseudocode:
    # from microsoftazurespacefx import PodManager
    # manager = PodManager()
    # pod = manager.get_pod(sandbox_id)
    # result = pod.exec(["bash", "-c", command])
    # exit_code = result.exit_code
    # stdout = result.stdout
    # stderr = result.stderr

    # Mock implementation - return success with empty output
    # In production, this would execute the actual command
    exit_code = 0
    stdout = ""
    stderr = ""

    logger.info("[%.2fs] Command completed with exit code %d", time.time() - t0, exit_code)

    result = {
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
    }
    sys.stdout.write(json.dumps(result) + "\n")
    sys.exit(exit_code)


@cli.command("destroy")
@click.argument("sandbox_id")
def destroy(sandbox_id: str):
    """Terminate an Azure Orbital sandbox/pod.

    SANDBOX_ID is the pod ID to terminate.
    """
    t0 = time.time()

    logger.info("[%.2fs] Terminating pod %s...", time.time() - t0, sandbox_id)

    # Implementation note: Actual Azure Orbital Space SDK implementation would:
    # 1. Connect to Kubernetes and delete the pod
    # 2. Wait for pod termination to complete
    # 3. Clean up any associated resources (Dapr components, etc.)
    #
    # Example pseudocode:
    # from microsoftazurespacefx import PodManager
    # manager = PodManager()
    # pod = manager.get_pod(sandbox_id)
    # pod.terminate()
    # pod.wait_until_terminated()

    logger.info("[%.2fs] Pod %s terminated", time.time() - t0, sandbox_id)


@cli.command("download")
@click.argument("sandbox_id")
@click.argument("paths", nargs=-1, required=True)
def download(sandbox_id: str, paths: tuple[str, ...]):
    """Download files from an Azure Orbital sandbox/pod.

    SANDBOX_ID is the pod ID to download from.

    PATHS are one or more path specifications in the format "remote_path:local_path".
    Each specification downloads the remote file to the local path.

    Examples:

        azure_orbital_sandbox.py download orbital-pod-abc123 "/tmp/junit.xml:./results/junit.xml"

        azure_orbital_sandbox.py download orbital-pod-abc123 "/app/out:./out" "/app/logs:./logs"
    """
    t0 = time.time()

    logger.info("[%.2fs] Downloading files from pod %s", time.time() - t0, sandbox_id)

    for path_spec in paths:
        if ":" not in path_spec:
            logger.error(
                "Invalid path format '%s', expected 'remote_path:local_path'", path_spec
            )
            sys.exit(1)

        remote_path, local_path = path_spec.split(":", 1)
        if not remote_path:
            logger.error("Empty remote path in '%s'", path_spec)
            sys.exit(1)
        if not local_path:
            logger.error("Empty local path in '%s'", path_spec)
            sys.exit(1)

        # Implementation note: Actual Azure Orbital Space SDK implementation would:
        # 1. Use kubectl cp or SDK to copy files from the pod
        # 2. Handle both files and directories
        # 3. Create local parent directories as needed
        #
        # Example pseudocode:
        # from microsoftazurespacefx import PodManager
        # manager = PodManager()
        # pod = manager.get_pod(sandbox_id)
        # try:
        #     data = pod.read_file(remote_path)
        #     local_parent = os.path.dirname(local_path) or "."
        #     os.makedirs(local_parent, exist_ok=True)
        #     with open(local_path, "wb") as f:
        #         f.write(data)
        # except FileNotFoundError:
        #     logger.error("Remote file not found: %s", remote_path)
        #     sys.exit(1)

        logger.info("[%.2fs] Would download %s -> %s", time.time() - t0, remote_path, local_path)

        # Create parent directory for the mock implementation
        local_parent = os.path.dirname(local_path.rstrip("/")) or "."
        os.makedirs(local_parent, exist_ok=True)

    logger.info("[%.2fs] Download complete", time.time() - t0)


if __name__ == "__main__":
    cli()
