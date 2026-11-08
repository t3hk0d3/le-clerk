//! Generic identity: the identity key, compared verbatim.

use serde::{Deserialize, Serialize};

use super::{Match, Signal};

/// Identity of an opaque resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenericIdentity {
    /// The identity key.
    pub identity_key: String,
}

impl GenericIdentity {
    /// Builds the identity from the settings' key.
    #[must_use]
    pub fn from_settings(identity_key: &str) -> Self {
        Self {
            identity_key: identity_key.to_owned(),
        }
    }
}

pub(super) fn matches(a: &GenericIdentity, b: &GenericIdentity) -> Match {
    if a.identity_key == b.identity_key {
        Match::Hit(Signal::IdentityKey)
    } else {
        Match::None
    }
}
