//! JUnit XML merging utilities.
//!
//! This module provides functions to merge multiple JUnit XML files into one.
//! Used to combine results from parallel test execution across multiple sandboxes.

use std::path::Path;

use tracing::{info, warn};

/// Merges multiple JUnit XML files into a single output file.
///
/// Reads all `.xml` files from `parts_dir`, combines their `<testsuite>` elements
/// into a single `<testsuites>` root, and writes to `output_path`.
///
/// # Arguments
///
/// * `parts_dir` - Directory containing JUnit XML part files
/// * `output_path` - Path to write the merged JUnit XML
///
/// # Returns
///
/// `Ok(())` on success, or an error if merging fails.
pub fn merge_junit_files(parts_dir: &Path, output_path: &Path) -> std::io::Result<()> {
    // Ensure output directory exists
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Find all XML files in parts directory
    let mut part_files: Vec<_> = std::fs::read_dir(parts_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "xml"))
        .map(|e| e.path())
        .collect();

    part_files.sort();

    if part_files.is_empty() {
        warn!("No JUnit XML files found in {}", parts_dir.display());
        // Write empty testsuites
        let empty = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="offload" tests="0" failures="0" errors="0" time="0">
</testsuites>"#;
        std::fs::write(output_path, empty)?;
        return Ok(());
    }

    info!(
        "Merging {} JUnit XML files from {}",
        part_files.len(),
        parts_dir.display()
    );

    // Read and collect all testsuite content
    let mut testsuites_content = Vec::new();
    let mut total_tests = 0;
    let mut total_failures = 0;
    let mut total_errors = 0;
    let mut total_time = 0.0;

    for path in &part_files {
        let content = std::fs::read_to_string(path)?;

        // Extract testsuite elements and their stats
        // Simple approach: find <testsuite> elements and copy them
        for line in content.lines() {
            if line.trim().starts_with("<testsuite") {
                // Parse stats from this testsuite
                if let Some(tests) = extract_attr(line, "tests") {
                    total_tests += tests.parse::<i32>().unwrap_or(0);
                }
                if let Some(failures) = extract_attr(line, "failures") {
                    total_failures += failures.parse::<i32>().unwrap_or(0);
                }
                if let Some(errors) = extract_attr(line, "errors") {
                    total_errors += errors.parse::<i32>().unwrap_or(0);
                }
                if let Some(time) = extract_attr(line, "time") {
                    total_time += time.parse::<f64>().unwrap_or(0.0);
                }
            }
        }

        // Extract everything between <testsuite and </testsuites> or end
        if let Some(start) = content.find("<testsuite") {
            let end = content
                .rfind("</testsuite>")
                .map(|i| i + "</testsuite>".len())
                .unwrap_or(content.len());
            testsuites_content.push(content[start..end].to_string());
        }
    }

    // Write merged output
    let mut output = String::new();
    output.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    output.push_str(&format!(
        "<testsuites name=\"offload\" tests=\"{}\" failures=\"{}\" errors=\"{}\" time=\"{:.3}\">\n",
        total_tests, total_failures, total_errors, total_time
    ));

    for suite in testsuites_content {
        // Indent the testsuite content
        for line in suite.lines() {
            output.push_str("  ");
            output.push_str(line);
            output.push('\n');
        }
    }

    output.push_str("</testsuites>\n");

    std::fs::write(output_path, output)?;
    info!("Wrote merged JUnit XML to {}", output_path.display());

    Ok(())
}

/// Extracts an attribute value from an XML tag.
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr);
    let start = tag.find(&pattern)? + pattern.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

/// Removes the parts directory after merging.
pub fn cleanup_parts(parts_dir: &Path) -> std::io::Result<()> {
    if parts_dir.exists() {
        std::fs::remove_dir_all(parts_dir)?;
        info!("Cleaned up JUnit parts directory: {}", parts_dir.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_attr() {
        let tag = r#"<testsuite name="test" tests="5" failures="1" time="1.23">"#;
        assert_eq!(extract_attr(tag, "tests"), Some("5".to_string()));
        assert_eq!(extract_attr(tag, "failures"), Some("1".to_string()));
        assert_eq!(extract_attr(tag, "time"), Some("1.23".to_string()));
        assert_eq!(extract_attr(tag, "missing"), None);
    }
}
