//! Validation for PHP INI directive keys and values before they reach the
//! canonical store or any rendered channel file.
//!
//! The denylist is load-bearing, not defense-in-depth: the MCP HTTP endpoint
//! on 127.0.0.1:9900 is reachable by other local users' processes, so a
//! directive that loads or executes code must never be accepted.

use std::fmt;

/// Directives that can load or execute code (or route mail through an
/// arbitrary binary). Checked ASCII-case-insensitively; no unsafe mode.
pub const DENIED_KEYS: [&str; 6] = [
    "extension",
    "zend_extension",
    "extension_dir",
    "auto_prepend_file",
    "auto_append_file",
    "sendmail_path",
];

const MAX_KEY_LEN: usize = 128;
const MAX_VALUE_LEN: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IniGuardError {
    InvalidKey { key: String, reason: &'static str },
    DeniedKey { key: String },
    InvalidValue { reason: &'static str },
}

impl fmt::Display for IniGuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IniGuardError::InvalidKey { key, reason } => {
                write!(f, "invalid PHP INI directive key '{key}': {reason}")
            }
            IniGuardError::DeniedKey { key } => write!(
                f,
                "PHP INI directive '{key}' is denied: it can load or execute \
                 code and must never be set through Hearth"
            ),
            IniGuardError::InvalidValue { reason } => {
                write!(f, "invalid PHP INI value: {reason}")
            }
        }
    }
}

impl std::error::Error for IniGuardError {}

/// Validate a directive key: INI-safe syntax, then the code-loading denylist.
pub fn validate_key(key: &str) -> Result<(), IniGuardError> {
    let mut chars = key.chars();
    let valid_first = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    let valid_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    if !valid_first || !valid_rest {
        return Err(IniGuardError::InvalidKey {
            key: key.to_string(),
            reason: "keys must start with a letter or underscore and contain \
                     only letters, digits, underscores, and dots",
        });
    }
    if key.len() > MAX_KEY_LEN {
        return Err(IniGuardError::InvalidKey {
            key: key.to_string(),
            reason: "keys are limited to 128 characters",
        });
    }
    let lowered = key.to_ascii_lowercase();
    if DENIED_KEYS.contains(&lowered.as_str()) {
        return Err(IniGuardError::DeniedKey {
            key: key.to_string(),
        });
    }
    Ok(())
}

/// Validate a directive value: bounded length, printable characters only
/// (no control chars — INI line injection), no leading `[` (section injection).
pub fn validate_value(value: &str) -> Result<(), IniGuardError> {
    if value.chars().count() > MAX_VALUE_LEN {
        return Err(IniGuardError::InvalidValue {
            reason: "values are limited to 1024 characters",
        });
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(IniGuardError::InvalidValue {
            reason: "values must not contain control characters (newlines, \
                     carriage returns, NUL, ...)",
        });
    }
    if value.trim_start().starts_with('[') {
        return Err(IniGuardError::InvalidValue {
            reason: "values must not start with '[' (INI section injection)",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_common_directives() {
        assert!(validate_key("memory_limit").is_ok());
        assert!(validate_key("opcache.enable").is_ok());
    }

    #[test]
    fn rejects_key_with_space() {
        assert!(validate_key("memory limit").is_err());
    }

    #[test]
    fn rejects_key_with_leading_digit() {
        assert!(validate_key("9lives").is_err());
    }

    #[test]
    fn rejects_key_too_long() {
        // 128 chars is the cap; 129 must fail.
        assert!(validate_key(&"a".repeat(128)).is_ok());
        assert!(validate_key(&"a".repeat(129)).is_err());
    }

    #[test]
    fn rejects_empty_key() {
        assert!(validate_key("").is_err());
    }

    #[test]
    fn accepts_reasonable_values() {
        assert!(validate_value("1G").is_ok());
        assert!(validate_value("").is_ok());
        assert!(validate_value("/tmp/some path/with spaces").is_ok());
    }

    #[test]
    fn rejects_value_with_newline() {
        assert!(validate_value("1G\nextension=evil.so").is_err());
    }

    #[test]
    fn rejects_value_with_carriage_return() {
        assert!(validate_value("1G\r").is_err());
    }

    #[test]
    fn rejects_value_with_nul() {
        assert!(validate_value("1G\0").is_err());
    }

    #[test]
    fn rejects_value_with_leading_bracket() {
        // A leading '[' could smuggle an INI section header.
        assert!(validate_value("[PHP]").is_err());
    }

    #[test]
    fn rejects_value_too_long() {
        assert!(validate_value(&"x".repeat(1024)).is_ok());
        assert!(validate_value(&"x".repeat(1025)).is_err());
    }

    #[test]
    fn rejects_all_denied_keys() {
        for key in [
            "extension",
            "zend_extension",
            "extension_dir",
            "auto_prepend_file",
            "auto_append_file",
            "sendmail_path",
        ] {
            let err = validate_key(key).unwrap_err();
            assert!(
                err.to_string().contains(key),
                "error must name the denied directive '{key}': {err}"
            );
        }
    }

    #[test]
    fn rejects_denied_keys_case_insensitively() {
        for key in [
            "Extension",
            "SENDMAIL_PATH",
            "Zend_Extension",
            "Auto_Prepend_File",
        ] {
            let err = validate_key(key).unwrap_err();
            assert!(
                err.to_string()
                    .to_ascii_lowercase()
                    .contains(&key.to_ascii_lowercase()),
                "error must name the denied directive '{key}': {err}"
            );
        }
    }
}
