//! Behaviour tests for identity extraction and matching.

use std::fs;
use std::net::IpAddr;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use leclerk::dedup::{
    DeviceIdentity, GenericIdentity, HostClass, Identity, Match, NodeKind, Signal, matches,
};
use leclerk::model::{ServiceMode, UnitSettings};
use tempfile::TempDir;

fn nosys() -> PathBuf {
    PathBuf::from("/nonexistent-sysfs")
}

fn dev(path: &Path, sys: &Path) -> Identity {
    Identity::from_settings(&UnitSettings::Device { path: path.into() }, sys)
}

fn svc(host: &str, port: u16, service_id: Option<&str>) -> Identity {
    Identity::from_settings(
        &UnitSettings::Service {
            mode: ServiceMode::Listen,
            scheme: None,
            host: host.into(),
            port,
            service_id: service_id.map(Into::into),
        },
        &nosys(),
    )
}

fn generic(key: &str) -> Identity {
    Identity::from_settings(
        &UnitSettings::Generic {
            identity_key: key.into(),
        },
        &nosys(),
    )
}

fn device(i: &Identity) -> &DeviceIdentity {
    match i {
        Identity::Device(d) => d,
        _ => panic!("not a device"),
    }
}

/// Writes `files` (relative path, content) under `root`, creating parents.
fn put(root: &Path, files: &[(&str, &str)]) {
    for (rel, content) in files {
        let f = root.join(rel);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, content).unwrap();
    }
}

/// Makes `<root>/dev/<kind>/<mm>` a symlink to the directory `<root>/<rel>`.
fn link(root: &Path, kind: &str, mm: &str, rel: &str) {
    let dir = root.join(rel);
    fs::create_dir_all(&dir).unwrap();
    fs::create_dir_all(root.join("dev").join(kind)).unwrap();
    symlink(&dir, root.join("dev").join(kind).join(mm)).unwrap();
}

/// Fake sysfs where char devices 1:3 (/dev/null) and 1:5 (/dev/zero) both sit
/// on interface 00 of one USB device with the given serial.
fn fake_sysfs(serial: Option<&str>) -> TempDir {
    let t = TempDir::new().unwrap();
    let usb = "devices/usb1/1-1";
    put(
        t.path(),
        &[
            ("devices/usb1/1-1/idVendor", "10c4\n"),
            ("devices/usb1/1-1/idProduct", "ea60\n"),
            ("devices/usb1/1-1/1-1:1.0/bInterfaceNumber", "00\n"),
        ],
    );
    if let Some(s) = serial {
        put(t.path(), &[("devices/usb1/1-1/serial", &format!("{s}\n"))]);
    }
    let tty = format!("{usb}/1-1:1.0/ttyUSB");
    link(t.path(), "char", "1:3", &tty);
    link(t.path(), "char", "1:5", &tty);
    t
}

mod device_identity_from_settings {
    use super::*;

    mod on_a_symlink {
        use super::*;

        #[test]
        fn canonical_path_is_the_symlink_target() {
            let t = TempDir::new().unwrap();
            let target = t.path().join("ttyUSB1");
            fs::write(&target, "").unwrap();
            let link = t.path().join("by-id-X");
            symlink(&target, &link).unwrap();
            let id = dev(&link, &nosys());
            assert_eq!(
                device(&id).canonical_path,
                fs::canonicalize(&target).unwrap()
            );
        }

        #[test]
        fn the_node_and_its_symlink_match_on_inode() {
            let t = TempDir::new().unwrap();
            let target = t.path().join("ttyUSB1");
            fs::write(&target, "").unwrap();
            let link = t.path().join("by-id-X");
            symlink(&target, &link).unwrap();
            assert_eq!(
                matches(&dev(&target, &nosys()), &dev(&link, &nosys())),
                Match::Hit(Signal::Inode)
            );
        }
    }

    mod on_a_char_device_with_sysfs_usb_ancestry {
        use super::*;

        #[test]
        fn captures_rdev_and_usb_triple() {
            let sys = fake_sysfs(Some("ABC123"));
            let id = dev(Path::new("/dev/null"), sys.path());
            let d = device(&id);
            assert!(d.rdev.is_some());
            let usb = d.usb.as_ref().unwrap();
            assert_eq!(
                (&*usb.vendor, &*usb.product, &*usb.serial),
                ("10c4", "ea60", "ABC123")
            );
            assert_eq!(usb.interface.as_deref(), Some("00"));
        }

        #[test]
        fn without_a_serial_file_usb_is_none() {
            let sys = fake_sysfs(None);
            assert_eq!(device(&dev(Path::new("/dev/null"), sys.path())).usb, None);
        }

        #[test]
        fn with_no_sysfs_tree_usb_is_none() {
            let id = dev(Path::new("/dev/null"), &nosys());
            assert_eq!(device(&id).usb, None);
            assert!(device(&id).rdev.is_some());
        }
    }

    mod on_a_regular_file_or_directory {
        use super::*;

        #[test]
        fn captures_canonical_path_and_inode_without_rdev() {
            let t = TempDir::new().unwrap();
            let f = t.path().join("f");
            fs::write(&f, "").unwrap();
            for p in [f.as_path(), t.path()] {
                let id = dev(p, &nosys());
                let d = device(&id);
                assert_eq!(d.canonical_path, fs::canonicalize(p).unwrap());
                assert!(d.inode.is_some());
                assert_eq!(d.rdev, None);
                assert_eq!(d.usb, None);
            }
        }
    }

    mod on_a_missing_path {
        use super::*;

        #[test]
        fn keeps_the_given_path_with_empty_fields() {
            let id = dev(Path::new("/dev/ttyNOPE9"), &nosys());
            let d = device(&id);
            assert_eq!(d.canonical_path, Path::new("/dev/ttyNOPE9"));
            assert_eq!((d.rdev, d.inode, d.usb.clone()), (None, None, None));
        }
    }
}

mod service_identity_from_settings {
    use super::*;

    mod on_loopback_hosts {
        use super::*;

        #[test]
        fn all_normalise_to_the_loopback_class() {
            for h in ["localhost", "127.0.0.1", "::1"] {
                match svc(h, 80, None) {
                    Identity::Service(s) => assert_eq!(s.host_class, HostClass::Loopback, "{h}"),
                    _ => panic!(),
                }
            }
        }
    }

    mod on_other_hosts {
        use super::*;

        #[test]
        fn wildcards_ips_and_names_are_classified() {
            let class = |h| match svc(h, 1, None) {
                Identity::Service(s) => s.host_class,
                _ => panic!(),
            };
            assert_eq!(class("0.0.0.0"), HostClass::Wildcard);
            assert_eq!(class("::"), HostClass::Wildcard);
            assert_eq!(class(""), HostClass::Wildcard);
            assert_eq!(
                class("192.168.1.2"),
                HostClass::Ip("192.168.1.2".parse::<IpAddr>().unwrap())
            );
            assert_eq!(class("Foo.Local"), HostClass::Name("foo.local".into()));
        }
    }

    mod on_wildcard_listen_vs_loopback {
        use super::*;

        #[test]
        fn reports_endpoint_overlap() {
            assert_eq!(
                matches(&svc("0.0.0.0", 3000, None), &svc("127.0.0.1", 3000, None)),
                Match::Hit(Signal::Endpoint)
            );
        }
    }

    mod on_loopback_spellings {
        use super::*;

        #[test]
        fn localhost_matches_127_0_0_1() {
            assert_eq!(
                matches(&svc("localhost", 80, None), &svc("127.0.0.1", 80, None)),
                Match::Hit(Signal::Endpoint)
            );
        }
    }

    mod on_same_host_different_port {
        use super::*;

        #[test]
        fn reports_no_match() {
            assert_eq!(
                matches(&svc("127.0.0.1", 80, None), &svc("127.0.0.1", 81, None)),
                Match::None
            );
        }
    }

    mod on_different_names_or_ips {
        use super::*;

        #[test]
        fn do_not_overlap() {
            assert_eq!(
                matches(&svc("a.example", 80, None), &svc("b.example", 80, None)),
                Match::None
            );
            assert_eq!(
                matches(&svc("10.0.0.1", 80, None), &svc("10.0.0.2", 80, None)),
                Match::None
            );
            assert_eq!(
                matches(&svc("10.0.0.1", 80, None), &svc("127.0.0.1", 80, None)),
                Match::None
            );
        }

        #[test]
        fn names_match_case_insensitively() {
            assert_eq!(
                matches(&svc("A.Example", 80, None), &svc("a.example", 80, None)),
                Match::Hit(Signal::Endpoint)
            );
        }
    }

    mod on_equal_service_id_with_different_host_and_port {
        use super::*;

        #[test]
        fn reports_a_service_id_match() {
            assert_eq!(
                matches(
                    &svc("a.example", 80, Some("db")),
                    &svc("b.example", 81, Some("db"))
                ),
                Match::Hit(Signal::ServiceId)
            );
        }

        #[test]
        fn a_service_id_on_one_side_only_does_not_match() {
            assert_eq!(
                matches(
                    &svc("a.example", 80, Some("db")),
                    &svc("b.example", 81, None)
                ),
                Match::None
            );
        }
    }
}

mod generic_identity {
    use super::*;

    #[test]
    fn on_equal_identity_key_matches() {
        assert_eq!(
            matches(&generic("k"), &generic("k")),
            Match::Hit(Signal::IdentityKey)
        );
        assert_eq!(GenericIdentity::from_settings("k").identity_key, "k");
    }

    #[test]
    fn on_different_identity_key_does_not_match() {
        assert_eq!(matches(&generic("k"), &generic("j")), Match::None);
    }
}

mod dedup_matches {
    use super::*;

    mod on_canonical_path_equal {
        use super::*;

        #[test]
        fn two_missing_equal_paths_hit_canonical_path() {
            let p = Path::new("/dev/ttyGONE1");
            assert_eq!(
                matches(&dev(p, &nosys()), &dev(p, &nosys())),
                Match::Hit(Signal::CanonicalPath)
            );
        }

        #[test]
        fn two_different_missing_paths_do_not_match() {
            assert_eq!(
                matches(
                    &dev(Path::new("/dev/ttyGONE1"), &nosys()),
                    &dev(Path::new("/dev/ttyGONE2"), &nosys())
                ),
                Match::None
            );
        }

        #[test]
        fn a_missing_path_does_not_match_an_existing_one() {
            let t = TempDir::new().unwrap();
            assert_eq!(
                matches(
                    &dev(t.path(), &nosys()),
                    &dev(Path::new("/dev/ttyGONE1"), &nosys())
                ),
                Match::None
            );
        }
    }

    mod on_rdev_equal {
        use super::*;

        #[test]
        fn hits_rdev_when_there_is_no_usb_data() {
            assert_eq!(
                matches(
                    &dev(Path::new("/dev/null"), &nosys()),
                    &dev(Path::new("/dev/null"), &nosys())
                ),
                Match::Hit(Signal::Rdev)
            );
        }
    }

    mod on_device_renumbered_with_same_usb_serial {
        use super::*;

        #[test]
        fn returns_usb_serial() {
            // /dev/null (1:3) stands in for ttyUSB1, /dev/zero (1:5) for ttyUSB2.
            let sys = fake_sysfs(Some("ABC123"));
            let before = dev(Path::new("/dev/null"), sys.path());
            let after = dev(Path::new("/dev/zero"), sys.path());
            assert_ne!(device(&before).rdev, device(&after).rdev);
            assert_eq!(matches(&before, &after), Match::Hit(Signal::UsbSerial));
        }

        #[test]
        fn different_serials_do_not_match() {
            let a = dev(Path::new("/dev/null"), fake_sysfs(Some("AAA")).path());
            let b = dev(Path::new("/dev/zero"), fake_sysfs(Some("BBB")).path());
            assert_eq!(matches(&a, &b), Match::None);
        }
    }

    mod on_different_types {
        use super::*;

        #[test]
        fn returns_none() {
            assert_eq!(
                matches(&generic("80"), &svc("localhost", 80, None)),
                Match::None
            );
            assert_eq!(
                matches(
                    &generic("/dev/null"),
                    &dev(Path::new("/dev/null"), &nosys())
                ),
                Match::None
            );
        }
    }

    mod on_round_trip {
        use super::*;

        #[test]
        fn identities_survive_serde_json() {
            let sys = fake_sysfs(Some("ABC123"));
            for id in [
                dev(Path::new("/dev/null"), sys.path()),
                svc("10.0.0.1", 80, Some("x")),
                svc("::1", 80, None),
                generic("k"),
            ] {
                let json = serde_json::to_string(&id).unwrap();
                assert_eq!(serde_json::from_str::<Identity>(&json).unwrap(), id);
            }
        }
    }
}

mod usb_lookup_fix_round_1 {
    use super::*;

    mod on_a_serial_less_device_below_a_serial_bearing_hub {
        use super::*;

        fn tree(device_dir: &str) -> TempDir {
            let t = TempDir::new().unwrap();
            put(
                t.path(),
                &[
                    ("devices/usb1/1-1/idVendor", "1d6b\n"),
                    ("devices/usb1/1-1/idProduct", "0002\n"),
                    ("devices/usb1/1-1/serial", "0000:00:14.0\n"),
                    ("devices/usb1/1-1/1-1.2/idVendor", "10c4\n"),
                    ("devices/usb1/1-1/1-1.2/idProduct", "ea60\n"),
                ],
            );
            link(t.path(), "char", "1:3", device_dir);
            link(t.path(), "char", "1:5", device_dir);
            t
        }

        #[test]
        fn usb_is_none_and_never_inherits_the_hub_triple() {
            let t = tree("devices/usb1/1-1/1-1.2/1-1.2:1.0/ttyUSB");
            let id = dev(Path::new("/dev/null"), t.path());
            assert_eq!(device(&id).usb, None);
        }

        #[test]
        fn two_such_devices_do_not_match_on_usb() {
            let t = tree("devices/usb1/1-1/1-1.2/1-1.2:1.0/ttyUSB");
            let a = dev(Path::new("/dev/null"), t.path());
            let b = dev(Path::new("/dev/zero"), t.path());
            assert_ne!(matches(&a, &b), Match::Hit(Signal::UsbSerial));
        }
    }

    mod on_a_multi_port_adapter {
        use super::*;

        fn adapter() -> TempDir {
            let t = TempDir::new().unwrap();
            put(
                t.path(),
                &[
                    ("devices/usb1/1-1/idVendor", "0403\n"),
                    ("devices/usb1/1-1/idProduct", "6010\n"),
                    ("devices/usb1/1-1/serial", "FT123\n"),
                    ("devices/usb1/1-1/1-1:1.0/bInterfaceNumber", "00\n"),
                    ("devices/usb1/1-1/1-1:1.1/bInterfaceNumber", "01\n"),
                ],
            );
            link(t.path(), "char", "1:3", "devices/usb1/1-1/1-1:1.0/ttyUSB0");
            link(t.path(), "char", "1:5", "devices/usb1/1-1/1-1:1.1/ttyUSB1");
            t
        }

        #[test]
        fn two_interfaces_of_one_device_do_not_hit_usb_serial() {
            let t = adapter();
            let a = dev(Path::new("/dev/null"), t.path());
            let b = dev(Path::new("/dev/zero"), t.path());
            assert_eq!(
                device(&a).usb.as_ref().unwrap().interface.as_deref(),
                Some("00")
            );
            assert_eq!(
                device(&b).usb.as_ref().unwrap().interface.as_deref(),
                Some("01")
            );
            assert_ne!(matches(&a, &b), Match::Hit(Signal::UsbSerial));
        }

        #[test]
        fn the_same_interface_renumbered_hits_usb_serial() {
            let t = adapter();
            put(t.path(), &[]);
            // 1:5 moves to interface 00 (renumbered node of the same port).
            fs::remove_file(t.path().join("dev/char/1:5")).unwrap();
            link(t.path(), "char", "1:5", "devices/usb1/1-1/1-1:1.0/ttyUSB2");
            let a = dev(Path::new("/dev/null"), t.path());
            let b = dev(Path::new("/dev/zero"), t.path());
            assert_eq!(matches(&a, &b), Match::Hit(Signal::UsbSerial));
        }
    }

    mod on_block_devices {
        use super::*;

        fn first_block_device() -> Option<PathBuf> {
            fs::read_dir("/dev")
                .ok()?
                .flatten()
                .map(|e| e.path())
                .find(|p| {
                    use std::os::unix::fs::FileTypeExt;
                    fs::metadata(p).is_ok_and(|m| m.file_type().is_block_device())
                })
        }

        #[test]
        fn lookup_uses_dev_block_not_dev_char() {
            let Some(node) = first_block_device() else {
                eprintln!("no block device in /dev; skipping");
                return;
            };
            let rdev = std::os::unix::fs::MetadataExt::rdev(&fs::metadata(&node).unwrap());
            let (maj, min) = (libc::major(rdev), libc::minor(rdev));
            let t = TempDir::new().unwrap();
            put(
                t.path(),
                &[
                    ("devices/usb1/1-1/idVendor", "0781\n"),
                    ("devices/usb1/1-1/idProduct", "5567\n"),
                    ("devices/usb1/1-1/serial", "BLK\n"),
                ],
            );
            link(
                t.path(),
                "block",
                &format!("{maj}:{min}"),
                "devices/usb1/1-1/sdx",
            );
            let id = dev(&node, t.path());
            assert_eq!(
                device(&id).usb.as_ref().map(|u| u.serial.as_str()),
                Some("BLK")
            );
        }
    }

    mod on_equal_rdev_with_different_node_kinds {
        use super::*;

        #[test]
        fn char_and_block_nodes_do_not_hit_rdev() {
            let id = |kind| {
                Identity::Device(DeviceIdentity {
                    canonical_path: PathBuf::from(format!("/dev/{kind:?}")),
                    rdev: Some(0x0803),
                    node_kind: Some(kind),
                    inode: None,
                    usb: None,
                })
            };
            assert_eq!(
                matches(&id(NodeKind::Char), &id(NodeKind::Block)),
                Match::None
            );
            assert_eq!(
                matches(&id(NodeKind::Char), &id(NodeKind::Char)),
                Match::Hit(Signal::Rdev)
            );
        }
    }
}
