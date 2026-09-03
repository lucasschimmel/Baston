//! Values that must never reach a log line.
//!
//! Three of the four values CFX returns are bearer credentials: presenting one
//! is sufficient to act as this server. They exist in this process because
//! there is nowhere else to keep them, but the type system can at least stop
//! them from being printed by accident — `tracing` fields, `anyhow` context
//! and `{:?}` on an enclosing struct all go through `Debug`.

use std::fmt;

/// A bearer credential. [`Debug`] and [`Display`](fmt::Display) both redact.
///
/// There is exactly one accessor, and it is named so that a reviewer can grep
/// for every place a secret leaves this module.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Build from a non-empty value, trimming whitespace the JSON boundary may
    /// have carried in. `None` for an empty value, so an absent credential is
    /// never mistaken for a present-but-blank one.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into().trim().to_owned();
        (!value.is_empty()).then_some(Self(value))
    }

    /// Expose the value at a protocol boundary. Every call site is a place a
    /// credential crosses into a request.
    #[must_use]
    pub fn expose_at_boundary(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_or_blank_value_is_not_a_secret() {
        assert!(Secret::new("").is_none());
        assert!(Secret::new("   \n").is_none());
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_rather_than_carried_into_a_header() {
        let s = Secret::new("  tok  ").expect("non-empty");
        assert_eq!(s.expose_at_boundary(), "tok");
    }

    #[test]
    fn neither_debug_nor_display_can_disclose_the_value() {
        let s = Secret::new("cfx-listing-token-value").expect("non-empty");
        assert_eq!(format!("{s:?}"), "Secret([REDACTED])");
        assert_eq!(format!("{s}"), "[REDACTED]");
        assert!(!format!("{s:?} {s}").contains("cfx-listing-token-value"));
    }

    #[test]
    fn a_struct_holding_one_cannot_leak_it_through_its_own_derive() {
        // The realistic accident: someone derives Debug on a config or state
        // struct and traces it. The field must redact itself from inside.
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Holder {
            name: String,
            token: Secret,
        }
        let h = Holder {
            name: "server".to_owned(),
            token: Secret::new("secret-value").unwrap(),
        };
        assert!(!format!("{h:?}").contains("secret-value"));
    }
}
