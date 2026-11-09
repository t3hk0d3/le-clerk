//! Domain model: identifiers and value types.

pub mod ids;
pub mod resource;
pub mod text;
pub mod types;

pub use ids::{IdError, LeaseId, NoticeId, PromotionId, RequestId, ResourceId, UnitId};
pub use resource::{Resource, Unit};
pub use text::{
    DESCRIPTION_MAX_CHARS, Description, RenderResource, RenderUnit, TextError, USAGE_MAX_BYTES,
    UsageTemplate, placeholder_keys,
};
pub use types::*;
