//! Domain model: identifiers and value types.

pub mod ids;
pub mod resource;
pub mod tag;
pub mod text;
pub mod types;

pub use ids::{IdError, LeaseId, NoticeId, PromotionId, RequestId, ResourceId, UnitId};
pub use resource::{Resource, Unit};
pub use tag::{TAG_MAX_CHARS, TAGS_MAX, Tag, TagError, TagSet};
pub use text::{
    DESCRIPTION_MAX_CHARS, Description, RenderResource, RenderUnit, TextError, USAGE_MAX_BYTES,
    UsageTemplate, placeholder_keys,
};
pub use types::*;
