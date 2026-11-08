//! Identity extraction and matching.
//!
//! Extraction is infallible and pure apart from reading the filesystem and an
//! injectable sysfs tree; matching is a pure comparison of two [`Identity`]
//! values. The store is not involved.

mod device;
mod generic;
mod service;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::UnitSettings;

pub use device::{DeviceIdentity, NodeKind, UsbId};
pub use generic::GenericIdentity;
pub use service::{HostClass, ServiceIdentity};

/// Default sysfs mount point.
pub const DEFAULT_SYSFS: &str = "/sys";

/// The comparable identity of a unit, stored in `units.identity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Identity {
    /// A device node.
    Device(DeviceIdentity),
    /// A network service.
    Service(ServiceIdentity),
    /// An opaque resource.
    Generic(GenericIdentity),
}

/// The signal that made two identities match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Signal {
    /// Same canonical device path.
    CanonicalPath,
    /// Same device number.
    Rdev,
    /// Same `(st_dev, st_ino)`.
    Inode,
    /// Same USB vendor, product and serial.
    UsbSerial,
    /// Same port on overlapping hosts.
    Endpoint,
    /// Same service id.
    ServiceId,
    /// Same generic identity key.
    IdentityKey,
}

/// Outcome of comparing two identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// Not the same thing.
    None,
    /// The same thing, reported by the first signal that hit.
    Hit(Signal),
}

impl Identity {
    /// Extracts the identity of `settings`, reading USB data below
    /// `sysfs_root` (normally [`DEFAULT_SYSFS`]). Never fails: whatever cannot
    /// be determined is left empty.
    #[must_use]
    pub fn from_settings(settings: &UnitSettings, sysfs_root: &Path) -> Identity {
        match settings {
            UnitSettings::Device { path } => {
                Identity::Device(DeviceIdentity::from_settings(path, sysfs_root))
            }
            UnitSettings::Service {
                mode,
                scheme,
                host,
                port,
                service_id,
            } => Identity::Service(ServiceIdentity::from_settings(
                *mode,
                scheme.as_deref(),
                host,
                *port,
                service_id.as_deref(),
            )),
            UnitSettings::Generic { identity_key } => {
                Identity::Generic(GenericIdentity::from_settings(identity_key))
            }
        }
    }
}

/// Compares two identities. Identities of different types never match.
#[must_use]
pub fn matches(a: &Identity, b: &Identity) -> Match {
    match (a, b) {
        (Identity::Device(a), Identity::Device(b)) => device::matches(a, b),
        (Identity::Service(a), Identity::Service(b)) => service::matches(a, b),
        (Identity::Generic(a), Identity::Generic(b)) => generic::matches(a, b),
        _ => Match::None,
    }
}
