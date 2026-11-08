//! Validated identifiers.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use ulid::Ulid;

use crate::error::ErrorCode;

/// Why an identifier was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    /// The text breaks the identifier's format rule.
    #[error("invalid id {value:?}: {rule}")]
    Invalid {
        /// The offending input.
        value: String,
        /// The rule it breaks.
        rule: &'static str,
    },
}

impl ErrorCode for IdError {
    fn code(&self) -> &'static str {
        match self {
            IdError::Invalid { .. } => "invalid_id",
        }
    }
}

const NAME_RULE: &str = "must match ^[a-z0-9][a-z0-9._-]{0,62}$";

fn valid_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    s.len() <= 63
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

macro_rules! name_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validate and wrap `s`.
            ///
            /// # Errors
            /// [`IdError::Invalid`] when `s` breaks the format rule.
            pub fn parse(s: &str) -> Result<Self, IdError> {
                if valid_name(s) {
                    Ok(Self(s.to_owned()))
                } else {
                    Err(IdError::Invalid { value: s.to_owned(), rule: NAME_RULE })
                }
            }

            /// The id as text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

macro_rules! ulid_id {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// The text prefix of this id kind.
            pub const PREFIX: &'static str = $prefix;

            /// A fresh id.
            #[must_use]
            pub fn new() -> Self {
                Self(format!("{}{}", $prefix, Ulid::new()))
            }

            /// Validate and wrap `s`.
            ///
            /// # Errors
            /// [`IdError::Invalid`] when the prefix or ULID part is wrong.
            pub fn parse(s: &str) -> Result<Self, IdError> {
                // Stored in canonical (uppercase) form so that ids differing
                // only in case compare equal.
                match s.strip_prefix($prefix).map(str::parse::<Ulid>) {
                    Some(Ok(ulid)) => Ok(Self(format!("{}{}", $prefix, ulid))),
                    _ => Err(IdError::Invalid {
                        value: s.to_owned(),
                        rule: concat!("must be `", $prefix, "` followed by a ULID"),
                    }),
                }
            }

            /// The id as text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

macro_rules! id_common {
    ($($name:ident),*) => {$(
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(s: String) -> Result<Self, IdError> {
                Self::parse(&s)
            }
        }
        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }
    )*};
}

name_id!(
    /// Identifier of a resource.
    ResourceId
);
name_id!(
    /// Identifier of a unit within a resource.
    UnitId
);
ulid_id!(
    /// Identifier of a lease (`lse_<ULID>`).
    LeaseId, "lse_"
);
ulid_id!(
    /// Identifier of a request (`req_<ULID>`).
    RequestId, "req_"
);
ulid_id!(
    /// Identifier of a promotion (`prm_<ULID>`).
    PromotionId, "prm_"
);
ulid_id!(
    /// Identifier of a notice (`ntc_<ULID>`).
    NoticeId, "ntc_"
);
id_common!(
    ResourceId,
    UnitId,
    LeaseId,
    RequestId,
    PromotionId,
    NoticeId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trips_and_validates() {
        let id = ResourceId::parse("esp32").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"esp32\"");
        assert!(serde_json::from_str::<ResourceId>("\"BAD\"").is_err());
    }

    #[test]
    fn error_code_is_snake_case() {
        let err = ResourceId::parse("").unwrap_err();
        assert_eq!(err.code(), "invalid_id");
    }
}
