use std::ffi::OsString;
use std::path::PathBuf;

use leclerk::config::{Config, ConfigError, Paths};
use leclerk::model::{
    BuiltinCheck, Check, HealthSpec, IdError, LeaseId, NoticeId, Priority, PromotionId, RequestId,
    ResourceId, ResourceType, ServiceMode, UnitId, UnitSettings,
};
mod ids {
    use super::*;

    mod resource_id_parse {
        use super::*;

        mod on_valid_id {
            use super::*;

            #[test]
            fn returns_the_id_unchanged() {
                let long = "a".repeat(63);
                for s in ["esp32-c6", "a", "0x.y_z", long.as_str()] {
                    assert_eq!(ResourceId::parse(s).unwrap().as_str(), s);
                    assert_eq!(UnitId::parse(s).unwrap().as_str(), s);
                }
            }
        }

        mod on_invalid_id {
            use super::*;

            #[test]
            fn returns_invalid_naming_the_rule() {
                let long = "a".repeat(64);
                for s in ["", long.as_str(), "ESP", "-a", "a/b", "é", ".a", "a b"] {
                    for err in [
                        ResourceId::parse(s).unwrap_err(),
                        UnitId::parse(s).unwrap_err(),
                    ] {
                        let IdError::Invalid { rule, .. } = &err;
                        assert!(rule.contains("[a-z0-9]"), "{err}");
                    }
                }
            }
        }
    }

    mod ulid_ids {
        use super::*;

        #[test]
        fn new_has_prefix_and_parses_back() {
            assert!(LeaseId::new().as_str().starts_with("lse_"));
            assert!(RequestId::new().as_str().starts_with("req_"));
            assert!(PromotionId::new().as_str().starts_with("prm_"));
            assert!(NoticeId::new().as_str().starts_with("ntc_"));
            let id = LeaseId::new();
            assert_eq!(LeaseId::parse(id.as_str()).unwrap(), id);
        }

        #[test]
        fn parse_rejects_wrong_prefix_and_bad_ulid() {
            let req = RequestId::new();
            assert!(LeaseId::parse(req.as_str()).is_err());
            assert!(LeaseId::parse("lse_nope").is_err());
            assert!(LeaseId::parse("").is_err());
        }

        #[test]
        fn parse_canonicalises_case() {
            let id = LeaseId::new();
            let lower = format!("lse_{}", id.as_str()[4..].to_lowercase());
            let parsed = LeaseId::parse(&lower).unwrap();
            assert_eq!(parsed, id);
            assert_eq!(parsed.as_str(), id.as_str());
        }

        #[test]
        fn new_ids_are_distinct() {
            assert_ne!(LeaseId::new(), LeaseId::new());
        }
    }
}

mod priority {
    use super::*;

    mod ordering {
        use super::*;

        mod on_comparison {
            use super::*;

            #[test]
            fn high_beats_normal_beats_low() {
                assert!(Priority::High > Priority::Normal);
                assert!(Priority::Normal > Priority::Low);
            }
        }
    }
}

mod config {
    use super::*;

    mod defaults {
        use super::*;
        use leclerk::config as c;

        #[test]
        fn equal_the_constants() {
            let cfg = Config::default();
            assert_eq!(cfg.warning_lead_max, c::WARNING_LEAD_MAX_SECS);
            assert!((cfg.warning_lead_ratio - c::WARNING_LEAD_RATIO).abs() < f64::EPSILON);
            assert_eq!(cfg.cleanup_window, c::CLEANUP_WINDOW_SECS);
            assert_eq!(cfg.cleanup_extension, c::CLEANUP_EXTENSION_SECS);
            assert_eq!(cfg.claim_window, c::CLAIM_WINDOW_SECS);
            assert_eq!(cfg.yield_window, c::YIELD_WINDOW_SECS);
            assert_eq!(cfg.health_timeout, c::HEALTH_TIMEOUT_SECS);
            assert_eq!(cfg.reconnect_grace, c::RECONNECT_GRACE_SECS);
        }

        #[test]
        fn have_the_documented_values() {
            let cfg = Config::default();
            assert_eq!(
                (
                    cfg.warning_lead_max,
                    cfg.cleanup_window,
                    cfg.cleanup_extension
                ),
                (300, 120, 120)
            );
            assert_eq!(
                (
                    cfg.claim_window,
                    cfg.yield_window,
                    cfg.health_timeout,
                    cfg.reconnect_grace
                ),
                (300, 120, 10, 60)
            );
        }
    }

    mod load {
        use super::*;

        fn paths_in(dir: &std::path::Path) -> Paths {
            Paths::from_lookup(|k| (k == "CLERK_HOME").then(|| dir.as_os_str().to_owned())).unwrap()
        }

        #[test]
        fn missing_file_gives_defaults() {
            let dir = tempfile::tempdir().unwrap();
            let cfg = Config::load(&paths_in(dir.path())).unwrap();
            assert_eq!(cfg, Config::default());
        }

        #[test]
        fn file_overrides_listed_keys_only() {
            let dir = tempfile::tempdir().unwrap();
            let paths = paths_in(dir.path());
            std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
            std::fs::write(&paths.config, "claim_window = 5\n").unwrap();
            let cfg = Config::load(&paths).unwrap();
            assert_eq!(cfg.claim_window, 5);
            assert_eq!(cfg.yield_window, Config::default().yield_window);
        }

        fn invalid_key(toml: &str) -> &'static str {
            let dir = tempfile::tempdir().unwrap();
            let paths = paths_in(dir.path());
            std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
            std::fs::write(&paths.config, toml).unwrap();
            match Config::load(&paths) {
                Err(ConfigError::Invalid { key }) => key,
                other => panic!("expected Invalid, got {other:?}"),
            }
        }

        #[test]
        fn rejects_a_nan_negative_or_above_one_warning_ratio() {
            for v in ["nan", "-0.1", "1.5"] {
                let toml = format!("warning_lead_ratio = {v}\n");
                assert_eq!(invalid_key(&toml), "warning_lead_ratio", "{v}");
            }
        }

        #[test]
        fn rejects_zero_windows_and_timeouts() {
            for key in [
                "cleanup_window",
                "claim_window",
                "yield_window",
                "health_timeout",
            ] {
                assert_eq!(invalid_key(&format!("{key} = 0\n")), key);
            }
        }

        #[test]
        fn accepts_a_ratio_of_zero_or_one() {
            for v in ["0.0", "1.0"] {
                let dir = tempfile::tempdir().unwrap();
                let paths = paths_in(dir.path());
                std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
                std::fs::write(&paths.config, format!("warning_lead_ratio = {v}\n")).unwrap();
                assert!(Config::load(&paths).is_ok(), "{v}");
            }
        }

        #[test]
        fn malformed_file_is_an_error() {
            let dir = tempfile::tempdir().unwrap();
            let paths = paths_in(dir.path());
            std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
            std::fs::write(&paths.config, "claim_window = [").unwrap();
            assert!(Config::load(&paths).is_err());
        }
    }

    mod paths {
        use super::*;

        #[test]
        fn clerk_home_puts_everything_under_it() {
            let home = PathBuf::from("/tmp/ch");
            let p =
                Paths::from_lookup(|k| (k == "CLERK_HOME").then(|| OsString::from(&home))).unwrap();
            for path in [&p.socket, &p.lock, &p.db, &p.log, &p.config] {
                assert!(path.starts_with(&home), "{}", path.display());
            }
            assert_eq!(p.socket, home.join("run/clerk.sock"));
            assert_eq!(p.lock, home.join("run/daemon.lock"));
            assert_eq!(p.db, home.join("state/clerk.db"));
            assert_eq!(p.log, home.join("state/daemon.log"));
            assert_eq!(p.config, home.join("config/config.toml"));
        }

        #[test]
        fn xdg_variables_are_honoured() {
            let p = Paths::from_lookup(|k| match k {
                "XDG_RUNTIME_DIR" => Some("/run/u".into()),
                "XDG_STATE_HOME" => Some("/st".into()),
                "XDG_CONFIG_HOME" => Some("/cf".into()),
                _ => None,
            })
            .unwrap();
            assert_eq!(p.socket, PathBuf::from("/run/u/clerk/clerk.sock"));
            assert_eq!(p.lock, PathBuf::from("/run/u/clerk/daemon.lock"));
            assert_eq!(p.db, PathBuf::from("/st/clerk/clerk.db"));
            assert_eq!(p.log, PathBuf::from("/st/clerk/daemon.log"));
            assert_eq!(p.config, PathBuf::from("/cf/clerk/config.toml"));
        }

        #[test]
        fn a_relative_clerk_home_is_made_absolute_against_the_cwd() {
            let p = Paths::from_lookup(|k| (k == "CLERK_HOME").then(|| "rel/home".into())).unwrap();
            let cwd = std::env::current_dir().unwrap();
            assert!(p.db.is_absolute(), "{}", p.db.display());
            assert_eq!(p.db, cwd.join("rel/home/state/clerk.db"));
        }

        #[test]
        fn no_home_and_no_xdg_variables_is_an_error() {
            let r = Paths::from_lookup(|_| None);
            assert!(matches!(r, Err(ConfigError::NoHome)), "{r:?}");
        }

        #[test]
        fn no_home_with_every_xdg_variable_is_fine() {
            let p = Paths::from_lookup(|k| match k {
                "XDG_RUNTIME_DIR" => Some("/run/u".into()),
                "XDG_STATE_HOME" => Some("/st".into()),
                "XDG_CONFIG_HOME" => Some("/cf".into()),
                _ => None,
            })
            .unwrap();
            assert_eq!(p.db, PathBuf::from("/st/clerk/clerk.db"));
        }

        #[test]
        fn defaults_fall_back_to_home_dirs() {
            let p = Paths::from_lookup(|k| (k == "HOME").then(|| "/home/x".into())).unwrap();
            assert_eq!(p.db, PathBuf::from("/home/x/.local/state/clerk/clerk.db"));
            assert_eq!(p.config, PathBuf::from("/home/x/.config/clerk/config.toml"));
        }
    }
}

mod unit_settings {
    use super::*;

    fn device(path: &str) -> UnitSettings {
        UnitSettings::Device { path: path.into() }
    }

    fn service(port: u16) -> UnitSettings {
        UnitSettings::Service {
            mode: ServiceMode::Connect,
            scheme: Some("http".into()),
            host: "127.0.0.1".into(),
            port,
            service_id: Some("s1".into()),
        }
    }

    mod validate {
        use super::*;

        #[test]
        fn rejects_port_zero() {
            assert!(service(0).validate(ResourceType::Service).is_err());
        }

        #[test]
        fn rejects_empty_path() {
            assert!(device("").validate(ResourceType::Device).is_err());
        }

        #[test]
        fn rejects_empty_host_and_identity_key() {
            let s = UnitSettings::Service {
                mode: ServiceMode::Listen,
                scheme: None,
                host: String::new(),
                port: 1,
                service_id: None,
            };
            assert!(s.validate(ResourceType::Service).is_err());
            let g = UnitSettings::Generic {
                identity_key: String::new(),
            };
            assert!(g.validate(ResourceType::Generic).is_err());
        }

        #[test]
        fn rejects_type_mismatch() {
            assert!(device("/dev/x").validate(ResourceType::Service).is_err());
        }

        #[test]
        fn accepts_valid_settings() {
            assert!(
                device("/dev/ttyUSB0")
                    .validate(ResourceType::Device)
                    .is_ok()
            );
            assert!(service(80).validate(ResourceType::Service).is_ok());
            let g = UnitSettings::Generic {
                identity_key: "k".into(),
            };
            assert!(g.validate(ResourceType::Generic).is_ok());
        }
    }

    mod env_vars {
        use super::*;

        #[test]
        fn names_are_upper_case_with_unit_prefix() {
            let g = UnitSettings::Generic {
                identity_key: "k".into(),
            };
            for s in [device("/dev/x"), service(80), g] {
                let vars = s.env_vars();
                for (k, _) in vars {
                    assert!(k.starts_with("CLERK_UNIT_"), "{k}");
                    assert_eq!(k, k.to_uppercase());
                }
            }
        }

        #[test]
        fn device_exports_path() {
            assert_eq!(
                device("/dev/x").env_vars(),
                vec![("CLERK_UNIT_PATH".to_owned(), "/dev/x".to_owned())]
            );
        }

        #[test]
        fn service_exports_host_and_port() {
            let vars = service(8080).env_vars();
            assert!(vars.contains(&("CLERK_UNIT_HOST".into(), "127.0.0.1".into())));
            assert!(vars.contains(&("CLERK_UNIT_PORT".into(), "8080".into())));
        }
    }
}

mod health_spec {
    use super::*;

    fn b(c: BuiltinCheck) -> Check {
        Check::Builtin(c)
    }

    mod default_for {
        use super::*;

        fn checks(t: ResourceType, m: Option<ServiceMode>) -> Vec<Check> {
            HealthSpec::default_for(t, m).checks
        }

        #[test]
        fn device_is_exists_and_not_open() {
            assert_eq!(
                checks(ResourceType::Device, None),
                vec![b(BuiltinCheck::Exists), b(BuiltinCheck::NotOpen)]
            );
        }

        #[test]
        fn service_depends_on_mode() {
            let t = ResourceType::Service;
            assert_eq!(
                checks(t, Some(ServiceMode::Connect)),
                vec![b(BuiltinCheck::TcpConnect)]
            );
            assert_eq!(
                checks(t, Some(ServiceMode::Listen)),
                vec![b(BuiltinCheck::PortFree)]
            );
            assert_eq!(checks(t, None), vec![b(BuiltinCheck::TcpConnect)]);
        }

        #[test]
        fn generic_is_none() {
            assert_eq!(
                checks(ResourceType::Generic, None),
                vec![b(BuiltinCheck::None)]
            );
        }

        #[test]
        fn defaults_validate_for_their_type() {
            for (t, m) in [
                (ResourceType::Device, None),
                (ResourceType::Service, Some(ServiceMode::Connect)),
                (ResourceType::Service, Some(ServiceMode::Listen)),
                (ResourceType::Generic, None),
            ] {
                assert!(HealthSpec::default_for(t, m).validate(t).is_ok());
            }
        }
    }

    mod validate {
        use super::*;

        fn spec(checks: Vec<Check>) -> HealthSpec {
            HealthSpec { checks }
        }

        #[test]
        fn rejects_builtin_not_allowed_for_type() {
            let s = spec(vec![b(BuiltinCheck::Http)]);
            assert!(s.validate(ResourceType::Device).is_err());
            assert!(s.validate(ResourceType::Generic).is_err());
            assert!(s.validate(ResourceType::Service).is_ok());
            let d = spec(vec![b(BuiltinCheck::Accessible)]);
            assert!(d.validate(ResourceType::Device).is_ok());
            assert!(d.validate(ResourceType::Service).is_err());
        }

        #[test]
        fn names_allowed_checks_in_kebab_case() {
            let s = spec(vec![b(BuiltinCheck::Http)]);
            let msg = s.validate(ResourceType::Device).unwrap_err().to_string();
            assert!(msg.contains("not offered by type device"), "{msg}");
            assert!(
                msg.contains("allowed: exists, accessible, not-open, none, custom"),
                "{msg}"
            );
        }

        #[test]
        fn names_the_offending_entry() {
            let s = spec(vec![b(BuiltinCheck::Exists), b(BuiltinCheck::Http)]);
            let msg = s.validate(ResourceType::Device).unwrap_err().to_string();
            assert_eq!(
                msg,
                "health check #2 'http' is not offered by type device; allowed: \
                 exists, accessible, not-open, none, custom"
            );
        }

        #[test]
        fn allows_custom_for_every_type() {
            let s = spec(vec![Check::Custom {
                command: "true".into(),
            }]);
            for t in [
                ResourceType::Device,
                ResourceType::Service,
                ResourceType::Generic,
            ] {
                assert!(s.validate(t).is_ok());
            }
        }

        #[test]
        fn rejects_empty_spec_and_blank_command() {
            assert!(spec(vec![]).validate(ResourceType::Generic).is_err());
            let s = spec(vec![Check::Custom {
                command: "  ".into(),
            }]);
            assert!(s.validate(ResourceType::Generic).is_err());
        }
    }

    mod serde_format {
        use super::*;

        #[test]
        fn matches_the_export_format() {
            let s = HealthSpec {
                checks: vec![
                    b(BuiltinCheck::TcpConnect),
                    Check::Custom {
                        command: "x".into(),
                    },
                ],
            };
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, r#"[{"builtin":"tcp-connect"},{"custom":"x"}]"#);
            assert_eq!(serde_json::from_str::<HealthSpec>(&json).unwrap(), s);
        }
    }
}
