//! What the client reads of the daemon's answers, leniently: a newer daemon
//! may add a notice kind, a field or a holder this client has never heard
//! of, and the notices it delivers are gone once delivered.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{LeaseId, NoticeId, RequestId, ResourceId, UnitId};

/// What a notice is about, as far as this client knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// A unit is offered; claim it before the deadline.
    Offered,
    /// A queued request (or an unclaimed offer) was dropped.
    QueueDropped,
    /// A queued request moved in its queue.
    PositionChanged,
    /// A lease is about to expire.
    ExpiryWarning,
    /// The cleanup window started.
    CleanupWindowStarted,
    /// The cleanup window is about to end.
    CleanupEnding,
    /// Someone asks the holder to yield.
    YieldRequested,
    /// A yield request was withdrawn: the holder need not yield.
    YieldWithdrawn,
    /// A lent-out unit is back.
    UnitReturned,
    /// A borrower ran past its declared duration.
    BorrowerOverran,
    /// The lease was overridden.
    Overridden,
    /// The lease ended.
    LeaseEnded,
    /// The resource or unit was removed.
    ResourceRemoved,
    /// A kind this client does not know: show its action text.
    #[serde(other)]
    Unknown,
}

/// One notice as the client reads it. The holder and the dirty record are
/// only passed along, so they stay as the daemon wrote them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeView {
    pub id: NoticeId,
    /// The recipient.
    pub holder: Value,
    pub kind: NoticeKind,
    #[serde(default)]
    pub lease_id: Option<LeaseId>,
    #[serde(default)]
    pub request_id: Option<RequestId>,
    #[serde(default)]
    pub resource: Option<ResourceId>,
    #[serde(default)]
    pub unit: Option<UnitId>,
    /// The other party's purpose; `None` when the recipient may not see it.
    #[serde(default)]
    pub other_purpose: Option<String>,
    /// The deadline this notice is about, unix ms.
    #[serde(default)]
    pub deadline: Option<i64>,
    /// The new queue position, for `PositionChanged`.
    #[serde(default)]
    pub position: Option<u32>,
    /// The unit's dirty record, when it is handed over dirty.
    #[serde(default)]
    pub dirty: Option<Value>,
    /// For `UnitReturned`: whether the borrower confirmed its cleanup.
    #[serde(default)]
    pub cleanup_confirmed: Option<bool>,
    /// What the recipient should do, in plain words.
    pub action: String,
    pub created_at: i64,
}

/// The notices in a batch answer. One that cannot be read at all is logged
/// and dropped, so the batch's others are not lost with it.
///
/// # Errors
/// When `batch` is not an array.
pub fn parse_notices(batch: Value) -> Result<Vec<NoticeView>, serde_json::Error> {
    let items: Vec<Value> = serde_json::from_value(batch)?;
    Ok(items
        .into_iter()
        .filter_map(|item| match serde_json::from_value(item) {
            Ok(notice) => Some(notice),
            Err(e) => {
                tracing::warn!(error = %e, "skipping a notice that cannot be read");
                None
            }
        })
        .collect())
}
