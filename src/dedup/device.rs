//! Device identity: canonical path, device number, inode and USB triple.

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Match, Signal};

/// USB vendor, product and serial of the device behind a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbId {
    /// `idVendor`.
    pub vendor: String,
    /// `idProduct`.
    pub product: String,
    /// `serial`.
    pub serial: String,
    /// `bInterfaceNumber` of the USB interface the node hangs off, if any.
    pub interface: Option<String>,
}

/// File type of a device node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    /// Character device.
    Char,
    /// Block device.
    Block,
}

/// Identity of a device node (or any filesystem path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    /// Symlink-resolved path; the given path when it does not exist.
    pub canonical_path: PathBuf,
    /// Device number, for character and block devices only.
    pub rdev: Option<u64>,
    /// File type behind `rdev`.
    pub node_kind: Option<NodeKind>,
    /// `(st_dev, st_ino)`; `None` when the path does not exist.
    pub inode: Option<(u64, u64)>,
    /// USB identity found through sysfs, if any.
    pub usb: Option<UsbId>,
}

impl DeviceIdentity {
    /// Extracts the identity of `path`, looking up USB data below
    /// `sysfs_root`. Infallible: a missing path or sysfs entry yields `None`
    /// fields, and a missing path keeps itself as `canonical_path` (an
    /// unplugged board may still be registered).
    #[must_use]
    pub fn from_settings(path: &Path, sysfs_root: &Path) -> Self {
        let canonical_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let Ok(meta) = fs::metadata(&canonical_path) else {
            return Self {
                canonical_path,
                rdev: None,
                node_kind: None,
                inode: None,
                usb: None,
            };
        };
        let ft = meta.file_type();
        let node_kind = if ft.is_char_device() {
            Some(NodeKind::Char)
        } else if ft.is_block_device() {
            Some(NodeKind::Block)
        } else {
            None
        };
        let rdev = node_kind.map(|_| meta.rdev());
        let usb = node_kind
            .zip(rdev)
            .and_then(|(kind, r)| usb_lookup(sysfs_root, kind, r));
        Self {
            canonical_path,
            rdev,
            node_kind,
            inode: Some((meta.dev(), meta.ino())),
            usb,
        }
    }
}

/// Walks up from `<sysfs>/dev/{char,block}/<major>:<minor>`, never leaving the
/// sysfs root. The first ancestor with `bInterfaceNumber` gives the interface;
/// the first ancestor with both `idVendor` and `idProduct` is the USB device
/// and ends the walk. Without a `serial` there, the result is `None` (a hub's
/// serial further up must not be inherited).
fn usb_lookup(sysfs_root: &Path, kind: NodeKind, rdev: u64) -> Option<UsbId> {
    let (major, minor) = (libc::major(rdev), libc::minor(rdev));
    let sub = match kind {
        NodeKind::Char => "char",
        NodeKind::Block => "block",
    };
    let link = sysfs_root.join(format!("dev/{sub}/{major}:{minor}"));
    let mut dir = fs::canonicalize(link).ok()?;
    let root = fs::canonicalize(sysfs_root).ok()?;
    let mut interface = None;
    while dir.starts_with(&root) && dir != root {
        let read = |name: &str| {
            fs::read_to_string(dir.join(name))
                .ok()
                .map(|s| s.trim().to_owned())
        };
        if interface.is_none() {
            interface = read("bInterfaceNumber");
        }
        if let (Some(vendor), Some(product)) = (read("idVendor"), read("idProduct")) {
            let serial = read("serial")?;
            return Some(UsbId {
                vendor,
                product,
                serial,
                interface,
            });
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Signals in priority order: USB id (vendor, product, serial, interface), device number, inode, path.
///
/// Inode and device number are compared only when both sides have one (device
/// numbers also only for the same node kind), so two
/// missing paths cannot match through absent data. Canonical paths are always
/// compared; for a missing path that is its given path, i.e. a literal
/// comparison.
pub(super) fn matches(a: &DeviceIdentity, b: &DeviceIdentity) -> Match {
    if a.usb.is_some() && a.usb == b.usb {
        return Match::Hit(Signal::UsbSerial);
    }
    if a.rdev.is_some() && a.rdev == b.rdev && a.node_kind == b.node_kind {
        return Match::Hit(Signal::Rdev);
    }
    if a.inode.is_some() && a.inode == b.inode {
        return Match::Hit(Signal::Inode);
    }
    if a.canonical_path == b.canonical_path {
        return Match::Hit(Signal::CanonicalPath);
    }
    Match::None
}
