//! Parse pytest group filter strings into typed components and compute
//! common-component hoisting across groups.

/// A typed component parsed from a pytest filter string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FilterComponent {
    /// `-m` / `--markexpr <expr>`
    Mark(String),
    /// `-k` / `--keyword <expr>`
    Keyword(String),
    /// `--deselect <prefix>`
    Deselect(String),
    /// `--ignore <path>`
    Ignore(String),
    /// `--ignore-glob <glob>`
    IgnoreGlob(String),
    /// Any unrecognized token.
    Unknown(String),
}

impl FilterComponent {
    /// Produce the canonical CLI flag form for this component.
    pub fn to_cli_arg(&self) -> (String, String) {
        let (flag, value) = match self {
            FilterComponent::Mark(v) => ("-m", v.as_str()),
            FilterComponent::Keyword(v) => ("-k", v.as_str()),
            FilterComponent::Deselect(v) => ("--deselect", v.as_str()),
            FilterComponent::Ignore(v) => ("--ignore", v.as_str()),
            FilterComponent::IgnoreGlob(v) => ("--ignore-glob", v.as_str()),
            FilterComponent::Unknown(v) => ("", v.as_str()),
        };
        (flag.to_string(), value.to_string())
    }
}

/// Errors that can occur while parsing a filter string.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The shell-words tokenization failed.
    #[error("Failed to tokenize filter string: {0}")]
    Tokenization(String),

    /// A known flag was not followed by a value token.
    #[error("Flag '{flag}' at position {position} requires a value")]
    MissingFlagValue { flag: String, position: usize },
}

/// Parse a single group's `filters` string into typed components.
///
/// Uses `shell_words::split` so quoted expressions are handled correctly.
pub fn parse_filter_string(filters: &str) -> Result<Vec<FilterComponent>, ParseError> {
    if filters.trim().is_empty() {
        return Ok(Vec::new());
    }

    let tokens =
        shell_words::split(filters).map_err(|e| ParseError::Tokenization(e.to_string()))?;

    let mut components = Vec::with_capacity(tokens.len());
    let mut i = 0;

    while i < tokens.len() {
        let token = &tokens[i];

        let maybe_pair = match token.as_str() {
            "-m" | "--markexpr" if i + 1 < tokens.len() => {
                Some((FilterComponent::Mark(tokens[i + 1].clone()), 2))
            }
            "-m" | "--markexpr" => {
                return Err(ParseError::MissingFlagValue {
                    flag: token.clone(),
                    position: i,
                });
            }
            "-k" | "--keyword" if i + 1 < tokens.len() => {
                Some((FilterComponent::Keyword(tokens[i + 1].clone()), 2))
            }
            "-k" | "--keyword" => {
                return Err(ParseError::MissingFlagValue {
                    flag: token.clone(),
                    position: i,
                });
            }
            "--deselect" if i + 1 < tokens.len() => {
                Some((FilterComponent::Deselect(tokens[i + 1].clone()), 2))
            }
            "--deselect" => {
                return Err(ParseError::MissingFlagValue {
                    flag: token.clone(),
                    position: i,
                });
            }
            "--ignore" if i + 1 < tokens.len() => {
                Some((FilterComponent::Ignore(tokens[i + 1].clone()), 2))
            }
            "--ignore" => {
                return Err(ParseError::MissingFlagValue {
                    flag: token.clone(),
                    position: i,
                });
            }
            "--ignore-glob" if i + 1 < tokens.len() => {
                Some((FilterComponent::IgnoreGlob(tokens[i + 1].clone()), 2))
            }
            "--ignore-glob" => {
                return Err(ParseError::MissingFlagValue {
                    flag: token.clone(),
                    position: i,
                });
            }
            _ => {
                components.push(FilterComponent::Unknown(token.clone()));
                None
            }
        };

        if let Some((component, advance)) = maybe_pair {
            components.push(component);
            i += advance;
        } else {
            i += 1;
        }
    }

    Ok(components)
}

/// Determine whether a group (represented by its parsed components) is
/// eligible for single-pass discovery.
///
/// A group is ineligible if it contains any `Unknown` component.
pub fn is_single_pass_eligible(components: &[FilterComponent]) -> bool {
    !components
        .iter()
        .any(|c| matches!(c, FilterComponent::Unknown(_)))
}

/// Hoist common filter components across all eligible groups.
///
/// Returns `None` if any group is ineligible (contains `Unknown`), or if
/// `group_components` is empty.
///
/// A component is hoisted iff it appears (same variant and exact same value)
/// in every group.
pub fn hoist_common_components(
    group_components: &[Vec<FilterComponent>],
) -> Option<Vec<FilterComponent>> {
    if group_components.is_empty() {
        return Some(Vec::new());
    }

    for components in group_components {
        if !is_single_pass_eligible(components) {
            return None;
        }
    }

    let first = &group_components[0];
    let mut hoisted = Vec::new();

    for candidate in first.iter() {
        let appears_in_all = group_components[1..]
            .iter()
            .all(|components| components.iter().any(|c| c == candidate));

        if appears_in_all {
            hoisted.push(candidate.clone());
        }
    }

    Some(hoisted)
}

/// Emit hoisted components back to a flat list of CLI argument strings.
pub fn emit_hoisted_args(components: &[FilterComponent]) -> Vec<String> {
    let mut args = Vec::with_capacity(components.len() * 2);

    for component in components {
        let (flag, value) = component.to_cli_arg();
        if flag.is_empty() {
            args.push(value);
        } else {
            args.push(flag);
            args.push(value);
        }
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty_filter_string() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("")?;
        assert!(components.is_empty());
        Ok(())
    }

    #[test]
    fn test_parse_whitespace_only_filter_string() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("   \n\t  ")?;
        assert!(components.is_empty());
        Ok(())
    }

    #[test]
    fn test_parse_mark_short() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("-m 'not slow'")?;
        assert_eq!(
            components,
            vec![FilterComponent::Mark("not slow".to_string())]
        );
        Ok(())
    }

    #[test]
    fn test_parse_mark_long() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("--markexpr 'not slow and not flaky'")?;
        assert_eq!(
            components,
            vec![FilterComponent::Mark("not slow and not flaky".to_string())]
        );
        Ok(())
    }

    #[test]
    fn test_parse_keyword_short() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("-k 'test_foo'")?;
        assert_eq!(
            components,
            vec![FilterComponent::Keyword("test_foo".to_string())]
        );
        Ok(())
    }

    #[test]
    fn test_parse_keyword_long() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("--keyword 'test_bar'")?;
        assert_eq!(
            components,
            vec![FilterComponent::Keyword("test_bar".to_string())]
        );
        Ok(())
    }

    #[test]
    fn test_parse_deselect() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("--deselect tests/test_foo.py")?;
        assert_eq!(
            components,
            vec![FilterComponent::Deselect("tests/test_foo.py".to_string())]
        );
        Ok(())
    }

    #[test]
    fn test_parse_ignore() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("--ignore tests/integration")?;
        assert_eq!(
            components,
            vec![FilterComponent::Ignore("tests/integration".to_string())]
        );
        Ok(())
    }

    #[test]
    fn test_parse_ignore_glob() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("--ignore-glob '*_legacy.py'")?;
        assert_eq!(
            components,
            vec![FilterComponent::IgnoreGlob("*_legacy.py".to_string())]
        );
        Ok(())
    }

    #[test]
    fn test_parse_unknown_token() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("--no-cov")?;
        assert_eq!(
            components,
            vec![FilterComponent::Unknown("--no-cov".to_string())]
        );
        Ok(())
    }

    #[test]
    fn test_parse_multiple_known_flags() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("-m 'not slow' --ignore tests/integration")?;
        assert_eq!(
            components,
            vec![
                FilterComponent::Mark("not slow".to_string()),
                FilterComponent::Ignore("tests/integration".to_string()),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_parse_known_and_unknown_mixed() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("-m 'not slow' --no-cov")?;
        assert_eq!(
            components,
            vec![
                FilterComponent::Mark("not slow".to_string()),
                FilterComponent::Unknown("--no-cov".to_string()),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_parse_quoted_value_with_spaces() -> Result<(), Box<dyn std::error::Error>> {
        let components = parse_filter_string("-m \"not slow and not flaky\"")?;
        assert_eq!(
            components,
            vec![FilterComponent::Mark("not slow and not flaky".to_string())]
        );
        Ok(())
    }

    #[test]
    fn test_parse_flag_without_value_returns_error() {
        let result = parse_filter_string("-m");
        assert!(
            matches!(result, Err(ParseError::MissingFlagValue { flag, position }) if flag == "-m" && position == 0),
        );
    }

    #[test]
    fn test_parse_deselect_without_value_returns_error() {
        let result = parse_filter_string("--deselect");
        assert!(
            matches!(result, Err(ParseError::MissingFlagValue { flag, position }) if flag == "--deselect" && position == 0),
        );
    }

    #[test]
    fn test_parse_invalid_shell_quoting() {
        let result = parse_filter_string("-m 'not slow");
        assert!(matches!(result, Err(ParseError::Tokenization(_))));
    }

    #[test]
    fn test_single_pass_eligible_with_known_only() {
        let components = vec![
            FilterComponent::Mark("not slow".to_string()),
            FilterComponent::Ignore("tests/integration".to_string()),
        ];
        assert!(is_single_pass_eligible(&components));
    }

    #[test]
    fn test_single_pass_ineligible_with_unknown() {
        let components = vec![
            FilterComponent::Mark("not slow".to_string()),
            FilterComponent::Unknown("--no-cov".to_string()),
        ];
        assert!(!is_single_pass_eligible(&components));
    }

    #[test]
    fn test_hoist_single_group() -> Result<(), Box<dyn std::error::Error>> {
        let group_components = vec![vec![FilterComponent::Mark("not slow".to_string())]];
        let hoisted = hoist_common_components(&group_components);
        assert_eq!(
            hoisted,
            Some(vec![FilterComponent::Mark("not slow".to_string())])
        );
        Ok(())
    }

    #[test]
    fn test_hoist_two_groups_with_common_component() -> Result<(), Box<dyn std::error::Error>> {
        let group_components = vec![
            vec![
                FilterComponent::Mark("not slow".to_string()),
                FilterComponent::Ignore("tests/integration".to_string()),
            ],
            vec![
                FilterComponent::Mark("not slow".to_string()),
                FilterComponent::Deselect("tests/test_old.py".to_string()),
            ],
        ];
        let hoisted = hoist_common_components(&group_components);
        assert_eq!(
            hoisted,
            Some(vec![FilterComponent::Mark("not slow".to_string())])
        );
        Ok(())
    }

    #[test]
    fn test_hoist_two_groups_with_no_common_component() -> Result<(), Box<dyn std::error::Error>> {
        let group_components = vec![
            vec![FilterComponent::Mark("not slow".to_string())],
            vec![FilterComponent::Mark("slow".to_string())],
        ];
        let hoisted = hoist_common_components(&group_components);
        assert_eq!(hoisted, Some(Vec::new()));
        Ok(())
    }

    #[test]
    fn test_hoist_three_groups() -> Result<(), Box<dyn std::error::Error>> {
        let group_components = vec![
            vec![
                FilterComponent::Mark("not slow".to_string()),
                FilterComponent::Ignore("tests/integration".to_string()),
            ],
            vec![
                FilterComponent::Mark("not slow".to_string()),
                FilterComponent::Ignore("tests/integration".to_string()),
            ],
            vec![
                FilterComponent::Mark("not slow".to_string()),
                FilterComponent::Ignore("tests/integration".to_string()),
            ],
        ];
        let hoisted = hoist_common_components(&group_components);
        assert_eq!(
            hoisted,
            Some(vec![
                FilterComponent::Mark("not slow".to_string()),
                FilterComponent::Ignore("tests/integration".to_string()),
            ])
        );
        Ok(())
    }

    #[test]
    fn test_hoist_with_ineligible_group_returns_none() {
        let group_components = vec![
            vec![FilterComponent::Mark("not slow".to_string())],
            vec![
                FilterComponent::Mark("not slow".to_string()),
                FilterComponent::Unknown("--no-cov".to_string()),
            ],
        ];
        let hoisted = hoist_common_components(&group_components);
        assert_eq!(hoisted, None);
    }

    #[test]
    fn test_hoist_empty_groups() {
        let hoisted = hoist_common_components(&[]);
        assert_eq!(hoisted, Some(Vec::new()));
    }

    #[test]
    fn test_hoist_homogeneous_value_must_match_exactly() -> Result<(), Box<dyn std::error::Error>> {
        let group_components = vec![
            vec![FilterComponent::Mark("a".to_string())],
            vec![FilterComponent::Mark("b".to_string())],
        ];
        let hoisted = hoist_common_components(&group_components);
        assert_eq!(hoisted, Some(Vec::new()));
        Ok(())
    }

    #[test]
    fn test_hoist_kinds_may_repeat() -> Result<(), Box<dyn std::error::Error>> {
        let group_components = vec![
            vec![
                FilterComponent::Deselect("a.py".to_string()),
                FilterComponent::Deselect("b.py".to_string()),
            ],
            vec![
                FilterComponent::Deselect("a.py".to_string()),
                FilterComponent::Deselect("b.py".to_string()),
            ],
        ];
        let hoisted = hoist_common_components(&group_components);
        assert_eq!(
            hoisted,
            Some(vec![
                FilterComponent::Deselect("a.py".to_string()),
                FilterComponent::Deselect("b.py".to_string()),
            ])
        );
        Ok(())
    }

    #[test]
    fn test_hoist_repeated_kind_partial_match() -> Result<(), Box<dyn std::error::Error>> {
        let group_components = vec![
            vec![
                FilterComponent::Deselect("a.py".to_string()),
                FilterComponent::Deselect("b.py".to_string()),
            ],
            vec![
                FilterComponent::Deselect("a.py".to_string()),
                FilterComponent::Deselect("c.py".to_string()),
            ],
        ];
        let hoisted = hoist_common_components(&group_components);
        assert_eq!(
            hoisted,
            Some(vec![FilterComponent::Deselect("a.py".to_string())])
        );
        Ok(())
    }

    #[test]
    fn test_emit_mark() {
        let component = FilterComponent::Mark("not slow".to_string());
        assert_eq!(
            component.to_cli_arg(),
            ("-m".to_string(), "not slow".to_string())
        );
    }

    #[test]
    fn test_emit_keyword() {
        let component = FilterComponent::Keyword("test_foo".to_string());
        assert_eq!(
            component.to_cli_arg(),
            ("-k".to_string(), "test_foo".to_string())
        );
    }

    #[test]
    fn test_emit_deselect() {
        let component = FilterComponent::Deselect("tests/test_foo.py".to_string());
        assert_eq!(
            component.to_cli_arg(),
            ("--deselect".to_string(), "tests/test_foo.py".to_string())
        );
    }

    #[test]
    fn test_emit_ignore() {
        let component = FilterComponent::Ignore("tests/integration".to_string());
        assert_eq!(
            component.to_cli_arg(),
            ("--ignore".to_string(), "tests/integration".to_string())
        );
    }

    #[test]
    fn test_emit_ignore_glob() {
        let component = FilterComponent::IgnoreGlob("*_legacy.py".to_string());
        assert_eq!(
            component.to_cli_arg(),
            ("--ignore-glob".to_string(), "*_legacy.py".to_string())
        );
    }

    #[test]
    fn test_emit_unknown() {
        let component = FilterComponent::Unknown("--no-cov".to_string());
        assert_eq!(
            component.to_cli_arg(),
            ("".to_string(), "--no-cov".to_string())
        );
    }

    #[test]
    fn test_emit_hoisted_args() {
        let components = vec![
            FilterComponent::Mark("not slow".to_string()),
            FilterComponent::Ignore("tests/integration".to_string()),
        ];
        let args = emit_hoisted_args(&components);
        assert_eq!(
            args,
            vec!["-m", "not slow", "--ignore", "tests/integration",]
        );
    }

    #[test]
    fn test_emit_hoisted_args_empty() {
        let args = emit_hoisted_args(&[]);
        assert!(args.is_empty());
    }

    #[test]
    fn test_component_equality() {
        let a = FilterComponent::Mark("foo".to_string());
        let b = FilterComponent::Mark("foo".to_string());
        let c = FilterComponent::Mark("bar".to_string());
        let d = FilterComponent::Keyword("foo".to_string());

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }
}
