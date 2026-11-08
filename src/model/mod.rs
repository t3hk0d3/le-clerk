//! Domain model: identifiers and value types.

pub mod ids;
pub mod resource;
pub mod types;

pub use ids::{IdError, LeaseId, NoticeId, PromotionId, RequestId, ResourceId, UnitId};
pub use resource::{Resource, Unit};
pub use types::*;
