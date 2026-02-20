//! JUnit XML test ID adapters.
//!
//! Different test frameworks store test identity differently in JUnit XML.
//! This module provides adapters to convert JUnit XML attributes (`classname`, `name`)
//! back to the framework's native test ID format.
//!
//! # The Problem
//!
//! When pytest discovers tests, it produces IDs like:
//! ```text
//! libs/mng/api/test_list.py::test_foo
//! ```
//!
//! But pytest's JUnit XML output stores this as:
//! ```xml
//! <testcase classname="libs.mng.api.test_list" name="test_foo" />
//! ```
//!
//! To match historical test durations for LPT scheduling, we need to convert
//! the JUnit format back to the discovery format.
//!
//! # Supported Formats
//!
//! | Format | Discovery ID | JUnit classname | JUnit name |
//! |--------|--------------|-----------------|------------|
//! | pytest | `path/to/test.py::func` | `path.to.test` | `func` |
//! | nextest | `mod::submod::func` | `crate` | `mod::submod::func` |
//! | default | `name` | (ignored) | `name` |

use serde::{Deserialize, Serialize};

/// Specifies how to convert JUnit XML attributes to test IDs.
///
/// Different test frameworks store test identity differently in JUnit XML.
/// This enum selects the appropriate conversion strategy.
///
/// # Example
///
/// ```toml
/// [offload]
/// max_parallel = 10
/// junit_format = "pytest"  # Use pytest-style conversion
/// ```
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JunitFormat {
    /// pytest format: `classname.replace('.', '/') + ".py::" + name`
    ///
    /// Converts:
    /// - classname: `libs.mng.api.test_list`
    /// - name: `test_foo`
    /// - Result: `libs/mng/api/test_list.py::test_foo`
    Pytest,

    /// cargo nextest format: just use `name` (already contains full path)
    ///
    /// Converts:
    /// - classname: `mycrate` (ignored)
    /// - name: `module::submodule::test_func`
    /// - Result: `module::submodule::test_func`
    Nextest,

    /// Default format: just use `name`
    ///
    /// This is a simple fallback that just returns the name attribute.
    /// Use this when your test runner's JUnit output already contains
    /// the full test ID in the name attribute.
    #[default]
    Default,
}

impl JunitFormat {
    /// Convert JUnit XML attributes to a test ID.
    ///
    /// # Arguments
    ///
    /// * `classname` - The `classname` attribute from `<testcase>`
    /// * `name` - The `name` attribute from `<testcase>`
    ///
    /// # Returns
    ///
    /// The test ID in the format expected by test discovery.
    pub fn to_test_id(&self, classname: &str, name: &str) -> String {
        match self {
            JunitFormat::Pytest => {
                // pytest discovery: path/to/test_file.py::test_function
                // JUnit XML: classname="path.to.test_file", name="test_function"
                format!("{}.py::{}", classname.replace('.', "/"), name)
            }
            JunitFormat::Nextest => {
                // nextest uses name as the full test path
                name.to_string()
            }
            JunitFormat::Default => {
                // Default: just use name
                name.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pytest_format() {
        let format = JunitFormat::Pytest;
        assert_eq!(
            format.to_test_id("libs.mng.api.test_list", "test_foo"),
            "libs/mng/api/test_list.py::test_foo"
        );
        assert_eq!(
            format.to_test_id("apps.changelings.cli.add_test", "test_bar"),
            "apps/changelings/cli/add_test.py::test_bar"
        );
    }

    #[test]
    fn test_nextest_format() {
        let format = JunitFormat::Nextest;
        assert_eq!(
            format.to_test_id("mycrate", "module::submodule::test_func"),
            "module::submodule::test_func"
        );
    }

    #[test]
    fn test_default_format() {
        let format = JunitFormat::Default;
        assert_eq!(format.to_test_id("foo.bar", "test_baz"), "test_baz");
        assert_eq!(format.to_test_id("", "test_only_name"), "test_only_name");
    }

    #[test]
    fn test_deserialize_format() {
        #[derive(Deserialize)]
        struct TestConfig {
            format: JunitFormat,
        }

        let pytest: TestConfig = toml::from_str(r#"format = "pytest""#).unwrap();
        assert_eq!(pytest.format, JunitFormat::Pytest);

        let nextest: TestConfig = toml::from_str(r#"format = "nextest""#).unwrap();
        assert_eq!(nextest.format, JunitFormat::Nextest);

        let default: TestConfig = toml::from_str(r#"format = "default""#).unwrap();
        assert_eq!(default.format, JunitFormat::Default);
    }
}
