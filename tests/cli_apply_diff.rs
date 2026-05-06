use std::fs;
use std::path::Path;
use std::process;

use anyhow::Context;
use assert_cmd::Command;
use tempfile::TempDir;

#[allow(deprecated)]
fn offload_cmd() -> anyhow::Result<Command> {
    Command::cargo_bin("offload").context("offload binary not found")
}

/// Initialize a minimal git repo in the given directory.
fn init_git_repo(dir: &Path) -> anyhow::Result<()> {
    let commands: &[&[&str]] = &[
        &["init", "-q"],
        &["config", "user.email", "test@test.com"],
        &["config", "user.name", "Test"],
    ];
    for args in commands {
        let status = process::Command::new("git")
            .args(*args)
            .current_dir(dir)
            .status()
            .context("failed to run git")?;
        anyhow::ensure!(status.success(), "git {:?} failed", args);
    }
    Ok(())
}

/// Run a git command in the given directory and assert success.
fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let status = process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .with_context(|| format!("failed to run git {:?}", args))?;
    anyhow::ensure!(status.success(), "git {:?} failed", args);
    Ok(())
}

/// Generate a binary patch from staged changes in the given git repo.
fn generate_patch(repo_dir: &Path) -> anyhow::Result<Vec<u8>> {
    let output = process::Command::new("git")
        .args(["diff", "--cached", "--binary"])
        .current_dir(repo_dir)
        .output()
        .context("git diff failed")?;
    anyhow::ensure!(
        output.status.success(),
        "git diff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

#[test]
fn test_apply_diff_new_text_file() -> anyhow::Result<()> {
    let source = TempDir::new()?;
    init_git_repo(source.path())?;

    // Create an initial empty commit so we have a base state.
    git(source.path(), &["commit", "--allow-empty", "-m", "init"])?;

    // Add a new text file and stage it.
    let file_content = "Hello, world!\nThis is a new file.\n";
    fs::write(source.path().join("greeting.txt"), file_content)?;
    git(source.path(), &["add", "greeting.txt"])?;

    // Generate the patch.
    let patch = generate_patch(source.path())?;

    // Set up the target directory (base state = empty, no .git).
    let target = TempDir::new()?;

    // Write the patch to a file.
    let patch_file = target.path().join("patch.diff");
    fs::write(&patch_file, &patch)?;

    // Apply the patch using offload apply-diff.
    offload_cmd()?
        .args([
            "apply-diff",
            patch_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "--project-root",
            target
                .path()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
        ])
        .assert()
        .success();

    // Verify the file was created with correct content.
    let result = fs::read_to_string(target.path().join("greeting.txt"))?;
    assert_eq!(result, file_content);

    Ok(())
}

#[test]
fn test_apply_diff_modified_text_file() -> anyhow::Result<()> {
    let source = TempDir::new()?;
    init_git_repo(source.path())?;

    // Create and commit an initial file.
    let original_content = "Line one\nLine two\nLine three\n";
    fs::write(source.path().join("data.txt"), original_content)?;
    git(source.path(), &["add", "data.txt"])?;
    git(source.path(), &["commit", "-m", "add data.txt"])?;

    // Modify the file and stage it.
    let modified_content = "Line one\nLine two MODIFIED\nLine three\nLine four\n";
    fs::write(source.path().join("data.txt"), modified_content)?;
    git(source.path(), &["add", "data.txt"])?;

    // Generate the patch.
    let patch = generate_patch(source.path())?;

    // Set up the target directory with the original committed content.
    let target = TempDir::new()?;
    fs::write(target.path().join("data.txt"), original_content)?;

    // Write the patch to a file.
    let patch_file = target.path().join("patch.diff");
    fs::write(&patch_file, &patch)?;

    // Apply the patch.
    offload_cmd()?
        .args([
            "apply-diff",
            patch_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "--project-root",
            target
                .path()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
        ])
        .assert()
        .success();

    // Verify the file has modified content.
    let result = fs::read_to_string(target.path().join("data.txt"))?;
    assert_eq!(result, modified_content);

    Ok(())
}

#[test]
fn test_apply_diff_deleted_file() -> anyhow::Result<()> {
    let source = TempDir::new()?;
    init_git_repo(source.path())?;

    // Create and commit a file.
    let content = "This file will be deleted.\n";
    fs::write(source.path().join("doomed.txt"), content)?;
    git(source.path(), &["add", "doomed.txt"])?;
    git(source.path(), &["commit", "-m", "add doomed.txt"])?;

    // Delete the file and stage it.
    git(source.path(), &["rm", "doomed.txt"])?;

    // Generate the patch.
    let patch = generate_patch(source.path())?;

    // Set up the target directory with the file present (base state).
    let target = TempDir::new()?;
    fs::write(target.path().join("doomed.txt"), content)?;

    // Write the patch to a file.
    let patch_file = target.path().join("patch.diff");
    fs::write(&patch_file, &patch)?;

    // Apply the patch.
    offload_cmd()?
        .args([
            "apply-diff",
            patch_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "--project-root",
            target
                .path()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
        ])
        .assert()
        .success();

    // Verify the file was deleted.
    assert!(
        !target.path().join("doomed.txt").exists(),
        "file should have been deleted"
    );

    Ok(())
}

#[test]
fn test_apply_diff_binary_file() -> anyhow::Result<()> {
    let source = TempDir::new()?;
    init_git_repo(source.path())?;

    // Create an initial empty commit.
    git(source.path(), &["commit", "--allow-empty", "-m", "init"])?;

    // Add a binary file (bytes that are not valid UTF-8).
    let binary_content: Vec<u8> = (0..=255).collect();
    fs::write(source.path().join("data.bin"), &binary_content)?;
    git(source.path(), &["add", "data.bin"])?;

    // Generate the patch.
    let patch = generate_patch(source.path())?;

    // Set up the target directory (empty base state).
    let target = TempDir::new()?;

    // Write the patch to a file.
    let patch_file = target.path().join("patch.diff");
    fs::write(&patch_file, &patch)?;

    // Apply the patch.
    offload_cmd()?
        .args([
            "apply-diff",
            patch_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "--project-root",
            target
                .path()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
        ])
        .assert()
        .success();

    // Verify the binary file exists with correct content.
    let result = fs::read(target.path().join("data.bin"))?;
    assert_eq!(result, binary_content);

    Ok(())
}

#[test]
fn test_apply_diff_multiple_files() -> anyhow::Result<()> {
    let source = TempDir::new()?;
    init_git_repo(source.path())?;

    // Create initial committed state: two files.
    let file_a_original = "File A original content\n";
    let file_b_content = "File B will be deleted\n";
    fs::write(source.path().join("file_a.txt"), file_a_original)?;
    fs::write(source.path().join("file_b.txt"), file_b_content)?;
    git(source.path(), &["add", "file_a.txt", "file_b.txt"])?;
    git(source.path(), &["commit", "-m", "initial files"])?;

    // Now make multiple changes in one patch:
    // 1. Modify file_a.txt
    let file_a_modified = "File A modified content\n";
    fs::write(source.path().join("file_a.txt"), file_a_modified)?;
    git(source.path(), &["add", "file_a.txt"])?;
    // 2. Delete file_b.txt (git rm stages the deletion automatically)
    git(source.path(), &["rm", "file_b.txt"])?;
    // 3. Create a new file_c.txt
    let file_c_content = "File C is brand new\n";
    fs::write(source.path().join("file_c.txt"), file_c_content)?;
    git(source.path(), &["add", "file_c.txt"])?;

    // Generate the combined patch.
    let patch = generate_patch(source.path())?;

    // Set up the target directory with the original committed state.
    let target = TempDir::new()?;
    fs::write(target.path().join("file_a.txt"), file_a_original)?;
    fs::write(target.path().join("file_b.txt"), file_b_content)?;

    // Write the patch to a file.
    let patch_file = target.path().join("patch.diff");
    fs::write(&patch_file, &patch)?;

    // Apply the patch.
    offload_cmd()?
        .args([
            "apply-diff",
            patch_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "--project-root",
            target
                .path()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
        ])
        .assert()
        .success();

    // Verify all changes applied correctly.
    let result_a = fs::read_to_string(target.path().join("file_a.txt"))?;
    assert_eq!(result_a, file_a_modified);

    assert!(
        !target.path().join("file_b.txt").exists(),
        "file_b.txt should have been deleted"
    );

    let result_c = fs::read_to_string(target.path().join("file_c.txt"))?;
    assert_eq!(result_c, file_c_content);

    Ok(())
}

#[test]
fn test_apply_diff_renamed_file() -> anyhow::Result<()> {
    let source = TempDir::new()?;
    init_git_repo(source.path())?;

    // Create a file inside a subdirectory and commit it.
    let content = "Hello from a subdirectory!\n";
    fs::create_dir_all(source.path().join("sub"))?;
    fs::write(source.path().join("sub/hello.txt"), content)?;
    git(source.path(), &["add", "sub/hello.txt"])?;
    git(source.path(), &["commit", "-m", "add sub/hello.txt"])?;

    // Rename the file within the same subdirectory.
    git(source.path(), &["mv", "sub/hello.txt", "sub/goodbye.txt"])?;

    // Generate the patch (rename is already staged by git mv).
    let patch = generate_patch(source.path())?;

    // Set up the target directory with the original file at its original path.
    let target = TempDir::new()?;
    fs::create_dir_all(target.path().join("sub"))?;
    fs::write(target.path().join("sub/hello.txt"), content)?;

    // Write the patch to a file.
    let patch_file = target.path().join("patch.diff");
    fs::write(&patch_file, &patch)?;

    // Apply the patch.
    offload_cmd()?
        .args([
            "apply-diff",
            patch_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "--project-root",
            target
                .path()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
        ])
        .assert()
        .success();

    // Verify the old file is gone and the new file exists with correct content.
    assert!(
        !target.path().join("sub/hello.txt").exists(),
        "original file sub/hello.txt should have been removed by the rename"
    );
    let result = fs::read_to_string(target.path().join("sub/goodbye.txt"))?;
    assert_eq!(result, content);

    Ok(())
}

#[test]
fn test_apply_diff_creates_parent_directories() -> anyhow::Result<()> {
    let source = TempDir::new()?;
    init_git_repo(source.path())?;

    // Create an initial empty commit.
    git(source.path(), &["commit", "--allow-empty", "-m", "init"])?;

    // Add a file in a deeply nested directory.
    let nested_path = "deeply/nested/dir/file.txt";
    let nested_content = "Content in a nested directory.\n";
    fs::create_dir_all(source.path().join("deeply/nested/dir"))?;
    fs::write(source.path().join(nested_path), nested_content)?;
    git(source.path(), &["add", nested_path])?;

    // Generate the patch.
    let patch = generate_patch(source.path())?;

    // Set up the target directory (empty, no nested dirs).
    let target = TempDir::new()?;

    // Write the patch to a file.
    let patch_file = target.path().join("patch.diff");
    fs::write(&patch_file, &patch)?;

    // Apply the patch.
    offload_cmd()?
        .args([
            "apply-diff",
            patch_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "--project-root",
            target
                .path()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
        ])
        .assert()
        .success();

    // Verify the file and all parent directories were created.
    let result = fs::read_to_string(target.path().join(nested_path))?;
    assert_eq!(result, nested_content);

    Ok(())
}
