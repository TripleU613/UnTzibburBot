//! Validation use cases (`ValidateDisplayNameUseCase`, `ValidateGroupNameUseCase`,
//! `ValidateMessageBodyUseCase`) and helpers for phone / OTP input.

use crate::constants::*;
use crate::models::code_points;
use serde::{Deserialize, Serialize};

/// Outcome of a text validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextValidation {
    /// Trimmed text.
    Valid {
        text: String,
    },
    Empty,
    TooLong {
        count: usize,
        max: usize,
    },
}

impl TextValidation {
    pub fn is_valid(&self) -> bool {
        matches!(self, TextValidation::Valid { .. })
    }
    pub fn into_valid(self) -> Option<String> {
        match self {
            TextValidation::Valid { text } => Some(text),
            _ => None,
        }
    }
}

fn validate_text(input: &str, max: usize) -> TextValidation {
    let text = input.trim();
    if text.is_empty() {
        return TextValidation::Empty;
    }
    let count = code_points(text);
    if count > max {
        return TextValidation::TooLong { count, max };
    }
    TextValidation::Valid {
        text: text.to_owned(),
    }
}

/// Trims, checks non-empty, max 64 code points.
pub fn validate_display_name(input: &str) -> TextValidation {
    validate_text(input, DEFAULT_MAX_DISPLAY_NAME)
}

/// Max 100 code points.
pub fn validate_group_name(input: &str) -> TextValidation {
    validate_text(input, DEFAULT_MAX_GROUP_NAME)
}

/// Max 2000 code points.
pub fn validate_message_body(input: &str) -> TextValidation {
    validate_text(input, DEFAULT_MAX_MESSAGE)
}

/// Composer state derived from a draft (`ComposerUiState.Enabled` fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerInfo {
    pub is_valid: bool,
    pub code_points: usize,
    pub is_near_limit: bool,
    pub max_length: usize,
}

pub fn composer_info(draft: &str) -> ComposerInfo {
    let cp = code_points(draft.trim());
    ComposerInfo {
        is_valid: cp > 0 && cp <= DEFAULT_MAX_MESSAGE,
        code_points: cp,
        is_near_limit: cp >= MESSAGE_NEAR_LIMIT,
        max_length: DEFAULT_MAX_MESSAGE,
    }
}

/// Cheap check that `s` looks like an E.164 phone number (`+` and 8–15 digits).
pub fn looks_like_e164(s: &str) -> bool {
    let s = s.trim();
    let Some(rest) = s.strip_prefix('+') else {
        return false;
    };
    (8..=15).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_digit())
}

/// Normalize user input (spaces, dashes, parentheses, leading `00`) to E.164-ish.
pub fn normalize_phone(s: &str) -> String {
    let mut digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.starts_with("00") {
        digits = digits[2..].to_owned();
    }
    format!("+{digits}")
}

/// Whether `code` is a well-formed 6-digit OTP.
pub fn is_valid_otp(code: &str) -> bool {
    code.len() == CODE_LENGTH && code.bytes().all(|b| b.is_ascii_digit())
}

/// Extract the first 6-digit run from an SMS body (`OtpMessageParser`).
pub fn extract_otp(sms: &str) -> Option<String> {
    let bytes = sms.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i - start == CODE_LENGTH {
                return Some(sms[start..i].to_owned());
            }
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_limits() {
        assert_eq!(
            validate_display_name("  Bob "),
            TextValidation::Valid { text: "Bob".into() }
        );
        assert_eq!(validate_display_name("   "), TextValidation::Empty);
        let long = "é".repeat(65);
        assert_eq!(
            validate_display_name(&long),
            TextValidation::TooLong { count: 65, max: 64 }
        );
        assert!(validate_group_name(&"x".repeat(100)).is_valid());
        assert!(!validate_message_body(&"x".repeat(2001)).is_valid());
        assert!(composer_info(&"x".repeat(1800)).is_near_limit);
    }

    #[test]
    fn phones_and_otp() {
        assert!(looks_like_e164("+972501234567"));
        assert!(!looks_like_e164("0501234567"));
        assert_eq!(normalize_phone("+972 (50) 123-4567"), "+972501234567");
        assert_eq!(normalize_phone("00972501234567"), "+972501234567");
        assert!(is_valid_otp("123456"));
        assert!(!is_valid_otp("12345"));
        assert_eq!(
            extract_otp("Your Tzibbur code is 482913. Expires in 5 min (id 12)").as_deref(),
            Some("482913")
        );
    }
}
