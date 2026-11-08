//! Shared error plumbing.

use serde_json::Value;

/// Errors that cross the API boundary expose a stable `snake_case` code.
pub trait ErrorCode {
    /// The machine-readable code, derived from the error variant.
    fn code(&self) -> &'static str;

    /// Structured data for codes that carry it (the `details` object of the
    /// wire error); `None` for the rest.
    fn details(&self) -> Option<Value> {
        None
    }
}
