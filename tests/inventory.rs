mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use leclerk::clock::ManualClock;
use leclerk::inventory::{
    Inventory, InventoryError, NewResource, NewUnit, PromotionError, Promotions, ResourcePatch,
    UnitPatch, View, Warning,
};
use leclerk::model::{
    BuiltinCheck, Caller, Check, HealthSpec, HolderRef, ProjectKey, Resource, ResourceId,
    ResourceType, Scope, UnitId, UnitSettings,
};
use leclerk::scope::{ProjectResolver, ScopeError};
use leclerk::store::{Store, audit};
use tempfile::TempDir;

fn rid(s: &str) -> ResourceId {
    ResourceId::parse(s).unwrap()
}

fn uid(s: &str) -> UnitId {
    UnitId::parse(s).unwrap()
}

fn project(p: &str) -> ProjectKey {
    ProjectKey(PathBuf::from(p))
}

fn caller(project_path: Option<&str>) -> Caller {
    Caller {
        holder: HolderRef::User {
            tag: "tester".into(),
        },
        project: project_path.map(project),
        override_reason: None,
    }
}

fn device_settings(path: &str) -> UnitSettings {
    UnitSettings::Device {
        path: PathBuf::from(path),
    }
}

fn new_device(id: &str, scope: Scope) -> NewResource {
    NewResource {
        id: rid(id),
        ty: ResourceType::Device,
        scope,
        description: "a board".into(),
        usage: String::new(),
        health: None,
        tags: Vec::new(),
    }
}

fn new_unit(id: &str, path: &str) -> NewUnit {
    NewUnit {
        id: uid(id),
        settings: device_settings(path),
        description: String::new(),
    }
}

struct Fixture {
    store: Store,
    inv: Inventory,
    promos: Promotions,
    clock: Arc<ManualClock>,
}

fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(1_000));
    Fixture {
        store: Store::open_in_memory().unwrap(),
        inv: Inventory::new(clock.clone()),
        promos: Promotions::new(clock.clone()),
        clock,
    }
}

impl Fixture {
    /// Create a device resource with one unit; returns the resource as stored.
    fn seed(&mut self, who: &Caller, id: &str, scope: Scope, unit: &str) -> Resource {
        self.store
            .tx(|tx| {
                self.inv.create_resource(tx, who, &new_device(id, scope))?;
                self.inv.add_unit(
                    tx,
                    who,
                    &rid(id),
                    &new_unit(unit, &format!("/dev/seed-{id}-{unit}")),
                )?;
                self.inv.get_resource(tx, who, &rid(id))
            })
            .unwrap()
    }

    fn audit_actions(&mut self) -> Vec<String> {
        self.store
            .tx(audit::all)
            .unwrap()
            .into_iter()
            .map(|e| e.action)
            .collect()
    }
}

mod store {
    use super::*;

    mod open {
        use super::*;

        mod on_a_file_path {
            use super::*;

            #[test]
            fn enables_wal_and_foreign_keys_and_persists() {
                let dir = TempDir::new().unwrap();
                let path = dir.path().join("sub").join("clerk.db");
                {
                    let mut store = Store::open(&path).unwrap();
                    let (mode, fk): (String, i64) = store
                        .tx(|tx| -> Result<_, leclerk::store::StoreError> {
                            Ok((
                                tx.conn()
                                    .query_row("PRAGMA journal_mode", [], |r| r.get(0))?,
                                tx.conn()
                                    .query_row("PRAGMA foreign_keys", [], |r| r.get(0))?,
                            ))
                        })
                        .unwrap();
                    assert_eq!(mode, "wal");
                    assert_eq!(fk, 1);
                    let who = caller(None);
                    let inv = Inventory::new(Arc::new(ManualClock::new(5)));
                    store
                        .tx(|tx| inv.create_resource(tx, &who, &new_device("a", Scope::User)))
                        .unwrap();
                }
                let mut store = Store::open(&path).unwrap();
                let inv = Inventory::new(Arc::new(ManualClock::new(5)));
                let got = store.tx(|tx| inv.get_resource(tx, &caller(None), &rid("a")));
                assert!(got.is_ok());
            }
        }
    }

    mod tx {
        use super::*;

        mod on_error {
            use super::*;

            #[test]
            fn rolls_back_all_changes() {
                let mut f = fixture();
                let who = caller(None);
                let res: Result<(), InventoryError> = f.store.tx(|tx| {
                    f.inv
                        .create_resource(tx, &who, &new_device("a", Scope::User))?;
                    Err(InventoryError::NotFound)
                });
                assert!(res.is_err());
                let got = f.store.tx(|tx| f.inv.get_resource(tx, &who, &rid("a")));
                assert!(matches!(got, Err(InventoryError::NotFound)));
            }
        }
    }
}

mod project_resolver {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["commit", "-q", "--allow-empty", "-m", "init"]);
        (tmp, root)
    }

    mod resolve {
        use super::*;

        mod on_cwd_inside_a_git_repo {
            use super::*;

            #[test]
            fn returns_the_canonical_common_git_dir() {
                let (_tmp, root) = repo();
                let sub = root.join("src").join("deep");
                std::fs::create_dir_all(&sub).unwrap();
                let key = ProjectResolver::resolve(&sub).unwrap();
                assert_eq!(key, ProjectKey(root.join(".git")));
            }
        }

        mod on_cwd_inside_a_linked_worktree {
            use super::*;

            #[test]
            fn returns_the_same_key_as_the_main_checkout() {
                let (tmp, root) = repo();
                let wt = tmp.path().canonicalize().unwrap().join("wt");
                git(
                    &root,
                    &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "side"],
                );
                let main_key = ProjectResolver::resolve(&root).unwrap();
                let wt_key = ProjectResolver::resolve(&wt).unwrap();
                assert_eq!(wt_key, main_key);
            }
        }

        mod on_cwd_outside_any_git_repo {
            use super::*;

            #[test]
            fn returns_the_canonical_cwd() {
                let tmp = TempDir::new().unwrap();
                let dir = tmp.path().canonicalize().unwrap();
                let key = ProjectResolver::resolve(&dir).unwrap();
                assert_eq!(key, ProjectKey(dir));
            }
        }

        mod on_nonexistent_cwd {
            use super::*;

            #[test]
            fn returns_no_such_directory() {
                let tmp = TempDir::new().unwrap();
                let err = ProjectResolver::resolve(&tmp.path().join("nope")).unwrap_err();
                assert!(matches!(err, ScopeError::NoSuchDirectory(_)));
            }
        }
    }
}

mod inventory {
    use super::*;

    mod create_resource {
        use super::*;

        mod on_new_id_in_scope {
            use super::*;

            fn create_with_health(f: &mut Fixture) -> Resource {
                let who = caller(Some("/p/a"));
                let health = HealthSpec {
                    checks: vec![Check::Builtin(BuiltinCheck::Exists)],
                };
                f.store
                    .tx(|tx| {
                        let mut n = new_device("esp32", Scope::Project(project("/p/a")));
                        n.usage = "bench 2".into();
                        n.health = Some(health);
                        f.inv.create_resource(tx, &who, &n).map(|c| c.value)
                    })
                    .unwrap()
            }

            #[test]
            fn persists_type_scope_description_and_health() {
                let mut f = fixture();
                create_with_health(&mut f);
                let who = caller(Some("/p/a"));
                let got = f
                    .store
                    .tx(|tx| f.inv.get_resource(tx, &who, &rid("esp32")))
                    .unwrap();
                assert_eq!(got.ty, ResourceType::Device);
                assert_eq!(got.scope, Scope::Project(project("/p/a")));
                assert_eq!(got.description, "a board");
                assert_eq!(got.usage, "bench 2");
                assert_eq!(
                    got.health,
                    Some(HealthSpec {
                        checks: vec![Check::Builtin(BuiltinCheck::Exists)]
                    })
                );
                assert_eq!(got.created_at, 1_000);
            }

            #[test]
            fn writes_audit_entry_resource_created() {
                let mut f = fixture();
                create_with_health(&mut f);
                let entries = f.store.tx(audit::all).unwrap();
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].action, "resource_created");
                assert_eq!(entries[0].at, 1_000);
                assert_eq!(
                    entries[0].actor,
                    HolderRef::User {
                        tag: "tester".into()
                    }
                );
                assert_eq!(entries[0].detail["id"], "esp32");
            }

            #[test]
            fn records_override_reason_reason_in_audit() {
                let mut f = fixture();
                let mut who = caller(None);
                who.override_reason = Some("user asked".into());
                f.store
                    .tx(|tx| {
                        f.inv
                            .create_resource(tx, &who, &new_device("a", Scope::User))
                    })
                    .unwrap();
                let entries = f.store.tx(audit::all).unwrap();
                assert_eq!(entries[0].override_reason.as_deref(), Some("user asked"));
            }
        }

        mod on_id_already_used_in_same_scope {
            use super::*;

            #[test]
            fn returns_already_exists() {
                let mut f = fixture();
                let who = caller(None);
                f.seed(&who, "a", Scope::User, "u1");
                let err = f
                    .store
                    .tx(|tx| {
                        f.inv
                            .create_resource(tx, &who, &new_device("a", Scope::User))
                    })
                    .unwrap_err();
                assert!(matches!(err, InventoryError::AlreadyExists));
            }
        }

        mod on_project_scope_other_than_the_callers {
            use super::*;

            #[test]
            fn returns_scope_not_allowed_and_persists_nothing() {
                let mut f = fixture();
                let who = caller(Some("/p/a"));
                let err = f
                    .store
                    .tx(|tx| {
                        f.inv.create_resource(
                            tx,
                            &who,
                            &new_device("x", Scope::Project(project("/p/b"))),
                        )
                    })
                    .unwrap_err();
                assert!(matches!(err, InventoryError::ScopeNotAllowed));
                let all = f
                    .store
                    .tx(|tx| f.inv.list_resources(tx, &who, View::All, &[]))
                    .unwrap();
                assert_eq!(all.len(), 0);
                assert_eq!(f.audit_actions().len(), 0);
            }

            #[test]
            fn does_not_reveal_whether_the_id_exists_there() {
                let mut f = fixture();
                f.seed(
                    &caller(Some("/p/b")),
                    "x",
                    Scope::Project(project("/p/b")),
                    "u1",
                );
                let err = f
                    .store
                    .tx(|tx| {
                        f.inv.create_resource(
                            tx,
                            &caller(Some("/p/a")),
                            &new_device("x", Scope::Project(project("/p/b"))),
                        )
                    })
                    .unwrap_err();
                assert!(matches!(err, InventoryError::ScopeNotAllowed));
            }
        }

        mod on_project_scope_with_no_caller_project {
            use super::*;

            #[test]
            fn returns_scope_not_allowed() {
                let mut f = fixture();
                let err = f
                    .store
                    .tx(|tx| {
                        f.inv.create_resource(
                            tx,
                            &caller(None),
                            &new_device("x", Scope::Project(project("/p/a"))),
                        )
                    })
                    .unwrap_err();
                assert!(matches!(err, InventoryError::ScopeNotAllowed));
            }
        }

        mod on_health_check_not_offered_by_the_type {
            use super::*;

            #[test]
            fn returns_invalid_settings_naming_health() {
                let mut f = fixture();
                let who = caller(None);
                let mut n = new_device("a", Scope::User);
                n.health = Some(HealthSpec {
                    checks: vec![Check::Builtin(BuiltinCheck::Http)],
                });
                let err = f
                    .store
                    .tx(|tx| f.inv.create_resource(tx, &who, &n))
                    .unwrap_err();
                assert!(matches!(
                    err,
                    InventoryError::InvalidSettings { field: "health" }
                ));
            }

            #[test]
            fn persists_nothing() {
                let mut f = fixture();
                let who = caller(None);
                let mut n = new_device("a", Scope::User);
                n.health = Some(HealthSpec {
                    checks: vec![Check::Builtin(BuiltinCheck::Http)],
                });
                let _ = f.store.tx(|tx| f.inv.create_resource(tx, &who, &n));
                let all = f
                    .store
                    .tx(|tx| f.inv.list_resources(tx, &who, View::All, &[]))
                    .unwrap();
                assert_eq!(all.len(), 0);
            }
        }

        mod on_project_resource_whose_id_equals_a_user_resource_id {
            use super::*;

            #[test]
            fn succeeds_with_shadows_user_resource_warning() {
                let mut f = fixture();
                let who = caller(Some("/p/a"));
                f.seed(&who, "esp", Scope::User, "u1");
                let created = f
                    .store
                    .tx(|tx| {
                        f.inv.create_resource(
                            tx,
                            &who,
                            &new_device("esp", Scope::Project(project("/p/a"))),
                        )
                    })
                    .unwrap();
                assert_eq!(
                    created.warnings,
                    vec![Warning::ShadowsUserResource { id: rid("esp") }]
                );
            }
        }

        mod on_user_resource_whose_id_equals_an_existing_project_resource_id {
            use super::*;

            #[test]
            fn succeeds_with_shadowed_by_project_warning() {
                let mut f = fixture();
                let who = caller(Some("/p/a"));
                f.seed(&who, "esp", Scope::Project(project("/p/a")), "u1");
                let created = f
                    .store
                    .tx(|tx| {
                        f.inv
                            .create_resource(tx, &who, &new_device("esp", Scope::User))
                    })
                    .unwrap();
                assert_eq!(
                    created.warnings,
                    vec![Warning::ShadowedByProject {
                        id: rid("esp"),
                        project: project("/p/a")
                    }]
                );
            }
        }
    }

    mod list_and_get_resource {
        use super::*;

        fn populated() -> Fixture {
            let mut f = fixture();
            f.seed(&caller(None), "user-board", Scope::User, "u1");
            f.seed(
                &caller(Some("/p/a")),
                "a-board",
                Scope::Project(project("/p/a")),
                "u1",
            );
            f.seed(
                &caller(Some("/p/b")),
                "b-board",
                Scope::Project(project("/p/b")),
                "u1",
            );
            f.seed(&caller(Some("/p/a")), "shared", Scope::User, "user-unit");
            f.seed(
                &caller(Some("/p/a")),
                "shared",
                Scope::Project(project("/p/a")),
                "proj-unit",
            );
            f
        }

        fn ids(list: &[Resource]) -> Vec<String> {
            let mut v: Vec<String> = list.iter().map(|r| r.id.as_str().to_owned()).collect();
            v.sort();
            v
        }

        mod on_caller_in_project_p {
            use super::*;

            #[test]
            fn returns_user_and_p_resources_never_other_projects() {
                let mut f = populated();
                let who = caller(Some("/p/a"));
                let list = f
                    .store
                    .tx(|tx| f.inv.list_resources(tx, &who, View::Caller, &[]))
                    .unwrap();
                assert_eq!(ids(&list), ["a-board", "shared", "shared", "user-board"]);
            }

            #[test]
            fn marks_p_resources_that_shadow_a_user_resource() {
                let mut f = populated();
                let who = caller(Some("/p/a"));
                let list = f
                    .store
                    .tx(|tx| f.inv.list_resources(tx, &who, View::Caller, &[]))
                    .unwrap();
                let flagged: Vec<(&str, bool)> = list
                    .iter()
                    .map(|r| (r.id.as_str(), r.shadows_user))
                    .collect();
                assert!(flagged.contains(&("shared", true)));
                assert!(flagged.contains(&("a-board", false)));
                assert!(flagged.contains(&("user-board", false)));
                let user_shared = list
                    .iter()
                    .find(|r| r.id.as_str() == "shared" && r.scope == Scope::User);
                assert!(!user_shared.unwrap().shadows_user);
            }

            #[test]
            fn resolves_a_shadowed_id_to_the_project_resource() {
                let mut f = populated();
                let who = caller(Some("/p/a"));
                let got = f
                    .store
                    .tx(|tx| f.inv.get_resource(tx, &who, &rid("shared")))
                    .unwrap();
                assert_eq!(got.scope, Scope::Project(project("/p/a")));
                assert_eq!(got.units[0].id, uid("proj-unit"));
            }

            #[test]
            fn does_not_resolve_other_projects_ids() {
                let mut f = populated();
                let who = caller(Some("/p/a"));
                let got = f
                    .store
                    .tx(|tx| f.inv.get_resource(tx, &who, &rid("b-board")));
                assert!(matches!(got, Err(InventoryError::NotFound)));
            }
        }

        mod on_caller_with_no_project {
            use super::*;

            #[test]
            fn returns_user_scope_resources_only() {
                let mut f = populated();
                let who = caller(None);
                let list = f
                    .store
                    .tx(|tx| f.inv.list_resources(tx, &who, View::Caller, &[]))
                    .unwrap();
                assert_eq!(ids(&list), ["shared", "user-board"]);
                assert!(list.iter().all(|r| r.scope == Scope::User));
            }
        }

        mod on_admin_view_all_scopes {
            use super::*;

            #[test]
            fn returns_every_resource_with_its_scope() {
                let mut f = populated();
                let who = caller(None);
                let list = f
                    .store
                    .tx(|tx| f.inv.list_resources(tx, &who, View::All, &[]))
                    .unwrap();
                assert_eq!(
                    ids(&list),
                    ["a-board", "b-board", "shared", "shared", "user-board"]
                );
                let b = list.iter().find(|r| r.id.as_str() == "b-board").unwrap();
                assert_eq!(b.scope, Scope::Project(project("/p/b")));
            }
        }
    }

    mod update_resource {
        use super::*;

        mod on_visible_resource {
            use super::*;

            #[test]
            fn applies_change_and_writes_audit_entry() {
                let mut f = fixture();
                let who = caller(None);
                f.seed(&who, "a", Scope::User, "u1");
                let patch = ResourcePatch {
                    description: Some("new text".into()),
                    usage: Some("n".into()),
                    health: Some(Some(HealthSpec {
                        checks: vec![Check::Builtin(BuiltinCheck::Accessible)],
                    })),
                    ..ResourcePatch::default()
                };
                let updated = f
                    .store
                    .tx(|tx| f.inv.update_resource(tx, &who, &rid("a"), patch))
                    .unwrap();
                assert_eq!(updated.description, "new text");
                assert_eq!(updated.usage, "n");
                assert!(updated.health.is_some());
                assert_eq!(f.audit_actions().last().unwrap(), "resource_updated");
            }

            #[test]
            fn leaves_unpatched_fields_alone() {
                let mut f = fixture();
                let who = caller(None);
                f.seed(&who, "a", Scope::User, "u1");
                let patch = ResourcePatch {
                    usage: Some("n".into()),
                    ..ResourcePatch::default()
                };
                let updated = f
                    .store
                    .tx(|tx| f.inv.update_resource(tx, &who, &rid("a"), patch))
                    .unwrap();
                assert_eq!(updated.description, "a board");
            }

            #[test]
            fn clears_health_with_some_none() {
                let mut f = fixture();
                let who = caller(None);
                let mut n = new_device("a", Scope::User);
                n.health = Some(HealthSpec {
                    checks: vec![Check::Builtin(BuiltinCheck::Exists)],
                });
                f.store
                    .tx(|tx| f.inv.create_resource(tx, &who, &n))
                    .unwrap();
                let patch = ResourcePatch {
                    health: Some(None),
                    ..ResourcePatch::default()
                };
                let updated = f
                    .store
                    .tx(|tx| f.inv.update_resource(tx, &who, &rid("a"), patch))
                    .unwrap();
                assert_eq!(updated.health, None);
            }

            #[test]
            fn rejects_a_health_spec_the_type_does_not_offer() {
                let mut f = fixture();
                let who = caller(None);
                f.seed(&who, "a", Scope::User, "u1");
                let patch = ResourcePatch {
                    health: Some(Some(HealthSpec {
                        checks: vec![Check::Builtin(BuiltinCheck::Http)],
                    })),
                    ..ResourcePatch::default()
                };
                let err = f
                    .store
                    .tx(|tx| f.inv.update_resource(tx, &who, &rid("a"), patch))
                    .unwrap_err();
                assert!(matches!(
                    err,
                    InventoryError::InvalidSettings { field: "health" }
                ));
            }
        }

        mod on_resource_invisible_to_caller {
            use super::*;

            #[test]
            fn returns_not_found() {
                let mut f = fixture();
                f.seed(
                    &caller(Some("/p/b")),
                    "b",
                    Scope::Project(project("/p/b")),
                    "u1",
                );
                let err = f
                    .store
                    .tx(|tx| {
                        f.inv.update_resource(
                            tx,
                            &caller(Some("/p/a")),
                            &rid("b"),
                            ResourcePatch::default(),
                        )
                    })
                    .unwrap_err();
                assert!(matches!(err, InventoryError::NotFound));
            }
        }
    }

    mod delete_resource {
        use super::*;

        mod on_visible_resource {
            use super::*;

            #[test]
            fn removes_it_with_its_units_and_writes_audit_entry() {
                let mut f = fixture();
                let who = caller(None);
                f.seed(&who, "a", Scope::User, "u1");
                f.store
                    .tx(|tx| f.inv.delete_resource(tx, &who, &rid("a")))
                    .unwrap();
                let got = f.store.tx(|tx| f.inv.get_resource(tx, &who, &rid("a")));
                assert!(matches!(got, Err(InventoryError::NotFound)));
                assert_eq!(f.audit_actions().last().unwrap(), "resource_deleted");
                let units: i64 = f
                    .store
                    .tx(|tx| -> Result<_, leclerk::store::StoreError> {
                        Ok(tx
                            .conn()
                            .query_row("SELECT count(*) FROM units", [], |r| r.get(0))?)
                    })
                    .unwrap();
                assert_eq!(units, 0);
            }
        }

        mod on_resource_invisible_to_caller {
            use super::*;

            #[test]
            fn returns_not_found() {
                let mut f = fixture();
                f.seed(
                    &caller(Some("/p/b")),
                    "b",
                    Scope::Project(project("/p/b")),
                    "u1",
                );
                let err = f
                    .store
                    .tx(|tx| f.inv.delete_resource(tx, &caller(None), &rid("b")))
                    .unwrap_err();
                assert!(matches!(err, InventoryError::NotFound));
            }
        }

        mod on_resource_with_held_units {
            use super::*;

            #[test]
            fn returns_busy_and_leaves_resource_unchanged() {
                let mut f = fixture();
                let who = caller(None);
                let r = f.seed(&who, "a", Scope::User, "u1");
                f.store
                    .tx(|tx| -> Result<(), InventoryError> {
                        common::insert_lease(tx, &r.units[0], "held");
                        Ok(())
                    })
                    .unwrap();
                let err = f
                    .store
                    .tx(|tx| f.inv.delete_resource(tx, &who, &rid("a")))
                    .unwrap_err();
                assert!(matches!(err, InventoryError::Busy));
                let still = f
                    .store
                    .tx(|tx| f.inv.get_resource(tx, &who, &rid("a")))
                    .unwrap();
                assert_eq!(still.units.len(), 1);
            }
        }

        mod on_resource_with_queued_requests {
            use super::*;

            #[test]
            fn returns_busy() {
                let mut f = fixture();
                let who = caller(None);
                let r = f.seed(&who, "a", Scope::User, "u1");
                f.store
                    .tx(|tx| -> Result<(), InventoryError> {
                        common::insert_queued_request(tx, &r, None);
                        Ok(())
                    })
                    .unwrap();
                let err = f
                    .store
                    .tx(|tx| f.inv.delete_resource(tx, &who, &rid("a")))
                    .unwrap_err();
                assert!(matches!(err, InventoryError::Busy));
            }
        }

        mod on_resource_whose_leases_all_ended {
            use super::*;

            #[test]
            fn deletes_it() {
                let mut f = fixture();
                let who = caller(None);
                let r = f.seed(&who, "a", Scope::User, "u1");
                f.store
                    .tx(|tx| -> Result<(), InventoryError> {
                        common::insert_lease(tx, &r.units[0], "ended");
                        Ok(())
                    })
                    .unwrap();
                f.store
                    .tx(|tx| f.inv.delete_resource(tx, &who, &rid("a")))
                    .unwrap();
            }
        }
    }

    mod add_unit {
        use super::*;

        fn base() -> (Fixture, Caller) {
            let mut f = fixture();
            let who = caller(None);
            f.store
                .tx(|tx| {
                    f.inv
                        .create_resource(tx, &who, &new_device("a", Scope::User))
                })
                .unwrap();
            (f, who)
        }

        mod on_settings_valid_for_the_resource_type {
            use super::*;

            #[test]
            fn persists_unit_and_returns_created_without_warnings() {
                let (mut f, who) = base();
                let mut n = new_unit("u1", "/dev/ttyUSB0");
                n.description = "left port".into();
                let created = f
                    .store
                    .tx(|tx| f.inv.add_unit(tx, &who, &rid("a"), &n))
                    .unwrap();
                assert_eq!(created.warnings.len(), 0);
                assert_eq!(created.value.settings, device_settings("/dev/ttyUSB0"));
                let got = f
                    .store
                    .tx(|tx| f.inv.get_resource(tx, &who, &rid("a")))
                    .unwrap();
                assert_eq!(got.units.len(), 1);
                assert_eq!(got.units[0].id, uid("u1"));
                assert_eq!(got.units[0].description, "left port");
                assert!(!got.units[0].retiring);
                assert!(got.units[0].dirty.is_none());
            }

            #[test]
            fn writes_audit_entry_unit_added() {
                let (mut f, who) = base();
                f.store
                    .tx(|tx| {
                        f.inv
                            .add_unit(tx, &who, &rid("a"), &new_unit("u1", "/dev/x"))
                    })
                    .unwrap();
                assert_eq!(f.audit_actions().last().unwrap(), "unit_added");
            }
        }

        mod on_unit_id_already_used_in_resource {
            use super::*;

            #[test]
            fn returns_already_exists() {
                let (mut f, who) = base();
                f.store
                    .tx(|tx| {
                        f.inv
                            .add_unit(tx, &who, &rid("a"), &new_unit("u1", "/dev/x"))
                    })
                    .unwrap();
                let err = f
                    .store
                    .tx(|tx| {
                        f.inv
                            .add_unit(tx, &who, &rid("a"), &new_unit("u1", "/dev/y"))
                    })
                    .unwrap_err();
                assert!(matches!(err, InventoryError::AlreadyExists));
            }
        }

        mod on_settings_invalid_for_type {
            use super::*;

            #[test]
            fn names_path_for_an_empty_device_path() {
                let (mut f, who) = base();
                let err = f
                    .store
                    .tx(|tx| f.inv.add_unit(tx, &who, &rid("a"), &new_unit("u1", "")))
                    .unwrap_err();
                assert!(matches!(
                    err,
                    InventoryError::InvalidSettings { field: "path" }
                ));
            }

            #[test]
            fn names_settings_for_a_type_mismatch() {
                let (mut f, who) = base();
                let n = NewUnit {
                    id: uid("u1"),
                    settings: UnitSettings::Generic {
                        identity_key: "k".into(),
                    },
                    description: String::new(),
                };
                let err = f
                    .store
                    .tx(|tx| f.inv.add_unit(tx, &who, &rid("a"), &n))
                    .unwrap_err();
                assert!(matches!(
                    err,
                    InventoryError::InvalidSettings { field: "settings" }
                ));
            }

            #[test]
            fn names_port_for_port_zero() {
                let mut f = fixture();
                let who = caller(None);
                let mut r = new_device("svc", Scope::User);
                r.ty = ResourceType::Service;
                f.store
                    .tx(|tx| f.inv.create_resource(tx, &who, &r))
                    .unwrap();
                let n = NewUnit {
                    id: uid("u1"),
                    settings: UnitSettings::Service {
                        mode: leclerk::model::ServiceMode::Connect,
                        scheme: None,
                        host: "localhost".into(),
                        port: 0,
                        service_id: None,
                    },
                    description: String::new(),
                };
                let err = f
                    .store
                    .tx(|tx| f.inv.add_unit(tx, &who, &rid("svc"), &n))
                    .unwrap_err();
                assert!(matches!(
                    err,
                    InventoryError::InvalidSettings { field: "port" }
                ));
            }
        }

        mod on_resource_invisible_to_caller {
            use super::*;

            #[test]
            fn returns_not_found() {
                let (mut f, _) = base();
                let err = f
                    .store
                    .tx(|tx| {
                        f.inv
                            .add_unit(tx, &caller(None), &rid("zzz"), &new_unit("u1", "/x"))
                    })
                    .unwrap_err();
                assert!(matches!(err, InventoryError::NotFound));
            }
        }
    }

    mod update_unit {
        use super::*;

        mod on_valid_settings {
            use super::*;

            #[test]
            fn persists_changes_and_writes_audit_entry() {
                let mut f = fixture();
                let who = caller(None);
                f.seed(&who, "a", Scope::User, "u1");
                let patch = UnitPatch {
                    settings: Some(device_settings("/dev/ttyUSB9")),
                    description: Some("moved".into()),
                };
                let updated = f
                    .store
                    .tx(|tx| f.inv.update_unit(tx, &who, &rid("a"), &uid("u1"), patch))
                    .unwrap();
                assert_eq!(updated.warnings.len(), 0);
                assert_eq!(updated.value.settings, device_settings("/dev/ttyUSB9"));
                assert_eq!(updated.value.description, "moved");
                assert_eq!(f.audit_actions().last().unwrap(), "unit_updated");
            }
        }

        mod on_invalid_settings {
            use super::*;

            #[test]
            fn returns_invalid_settings_and_keeps_old_value() {
                let mut f = fixture();
                let who = caller(None);
                f.seed(&who, "a", Scope::User, "u1");
                let patch = UnitPatch {
                    settings: Some(device_settings("")),
                    description: None,
                };
                let err = f
                    .store
                    .tx(|tx| f.inv.update_unit(tx, &who, &rid("a"), &uid("u1"), patch))
                    .unwrap_err();
                assert!(matches!(
                    err,
                    InventoryError::InvalidSettings { field: "path" }
                ));
                let got = f
                    .store
                    .tx(|tx| f.inv.get_resource(tx, &who, &rid("a")))
                    .unwrap();
                assert_eq!(got.units[0].settings, device_settings("/dev/seed-a-u1"));
            }
        }

        mod on_unknown_unit {
            use super::*;

            #[test]
            fn returns_not_found() {
                let mut f = fixture();
                let who = caller(None);
                f.seed(&who, "a", Scope::User, "u1");
                let err = f
                    .store
                    .tx(|tx| {
                        f.inv
                            .update_unit(tx, &who, &rid("a"), &uid("nope"), UnitPatch::default())
                    })
                    .unwrap_err();
                assert!(matches!(err, InventoryError::NotFound));
            }
        }
    }

    mod remove_unit {
        use super::*;

        mod on_free_unit {
            use super::*;

            #[test]
            fn deletes_unit_and_its_links() {
                let mut f = fixture();
                let who = caller(None);
                let r = f.seed(&who, "a", Scope::User, "u1");
                let other = f.seed(&who, "b", Scope::User, "u2");
                let (a, b) = (r.units[0].row, other.units[0].row);
                f.store
                    .tx(|tx| -> Result<_, leclerk::store::StoreError> {
                        Ok(tx.conn().execute(
                            "INSERT INTO unit_links (unit_a, unit_b) VALUES (?1, ?2)",
                            [a, b],
                        )?)
                    })
                    .unwrap();
                f.store
                    .tx(|tx| f.inv.remove_unit(tx, &who, &rid("a"), &uid("u1")))
                    .unwrap();
                let got = f
                    .store
                    .tx(|tx| f.inv.get_resource(tx, &who, &rid("a")))
                    .unwrap();
                assert_eq!(got.units.len(), 0);
                let links: i64 = f
                    .store
                    .tx(|tx| -> Result<_, leclerk::store::StoreError> {
                        Ok(tx
                            .conn()
                            .query_row("SELECT count(*) FROM unit_links", [], |r| r.get(0))?)
                    })
                    .unwrap();
                assert_eq!(links, 0);
                assert_eq!(f.audit_actions().last().unwrap(), "unit_removed");
            }
        }

        mod on_held_unit {
            use super::*;

            #[test]
            fn returns_busy_and_keeps_the_unit() {
                let mut f = fixture();
                let who = caller(None);
                let r = f.seed(&who, "a", Scope::User, "u1");
                f.store
                    .tx(|tx| -> Result<(), InventoryError> {
                        common::insert_lease(tx, &r.units[0], "cleaning");
                        Ok(())
                    })
                    .unwrap();
                let err = f
                    .store
                    .tx(|tx| f.inv.remove_unit(tx, &who, &rid("a"), &uid("u1")))
                    .unwrap_err();
                assert!(matches!(err, InventoryError::Busy));
                let got = f
                    .store
                    .tx(|tx| f.inv.get_resource(tx, &who, &rid("a")))
                    .unwrap();
                assert_eq!(got.units.len(), 1);
            }
        }

        mod on_queued_unit {
            use super::*;

            #[test]
            fn returns_busy() {
                let mut f = fixture();
                let who = caller(None);
                let r = f.seed(&who, "a", Scope::User, "u1");
                f.store
                    .tx(|tx| -> Result<(), InventoryError> {
                        common::insert_queued_request(tx, &r, Some(&r.units[0]));
                        Ok(())
                    })
                    .unwrap();
                let err = f
                    .store
                    .tx(|tx| f.inv.remove_unit(tx, &who, &rid("a"), &uid("u1")))
                    .unwrap_err();
                assert!(matches!(err, InventoryError::Busy));
            }
        }
    }

    mod retire_unit {
        use super::*;

        mod on_free_unit {
            use super::*;

            #[test]
            fn removes_unit_immediately() {
                let mut f = fixture();
                let who = caller(None);
                f.seed(&who, "a", Scope::User, "u1");
                f.store
                    .tx(|tx| f.inv.retire_unit(tx, &who, &rid("a"), &uid("u1")))
                    .unwrap();
                let got = f
                    .store
                    .tx(|tx| f.inv.get_resource(tx, &who, &rid("a")))
                    .unwrap();
                assert_eq!(got.units.len(), 0);
                assert_eq!(f.audit_actions().last().unwrap(), "unit_retired");
            }
        }

        mod on_held_unit {
            use super::*;

            #[test]
            fn marks_retiring_and_keeps_the_unit() {
                let mut f = fixture();
                let who = caller(None);
                let r = f.seed(&who, "a", Scope::User, "u1");
                f.store
                    .tx(|tx| -> Result<(), InventoryError> {
                        common::insert_lease(tx, &r.units[0], "held");
                        Ok(())
                    })
                    .unwrap();
                f.store
                    .tx(|tx| f.inv.retire_unit(tx, &who, &rid("a"), &uid("u1")))
                    .unwrap();
                let got = f
                    .store
                    .tx(|tx| f.inv.get_resource(tx, &who, &rid("a")))
                    .unwrap();
                assert_eq!(got.units.len(), 1);
                assert!(got.units[0].retiring);
                assert_eq!(f.audit_actions().last().unwrap(), "unit_retired");
            }
        }

        mod on_unknown_unit {
            use super::*;

            #[test]
            fn returns_not_found() {
                let mut f = fixture();
                let who = caller(None);
                f.seed(&who, "a", Scope::User, "u1");
                let err = f
                    .store
                    .tx(|tx| f.inv.retire_unit(tx, &who, &rid("a"), &uid("nope")))
                    .unwrap_err();
                assert!(matches!(err, InventoryError::NotFound));
            }
        }
    }

    mod clock_use {
        use super::*;

        #[test]
        fn audit_timestamps_come_from_the_injected_clock() {
            let mut f = fixture();
            let who = caller(None);
            f.clock.set(777);
            f.seed(&who, "a", Scope::User, "u1");
            let entries = f.store.tx(audit::all).unwrap();
            assert!(entries.iter().all(|e| e.at == 777));
        }
    }

    mod cross_project_and_failed_mutations {
        use super::*;

        fn other_project() -> Fixture {
            let mut f = fixture();
            f.seed(
                &caller(Some("/p/b")),
                "b",
                Scope::Project(project("/p/b")),
                "u1",
            );
            f
        }

        #[test]
        fn update_unit_on_invisible_resource_returns_not_found() {
            let mut f = other_project();
            let err = f
                .store
                .tx(|tx| {
                    f.inv.update_unit(
                        tx,
                        &caller(Some("/p/a")),
                        &rid("b"),
                        &uid("u1"),
                        UnitPatch::default(),
                    )
                })
                .unwrap_err();
            assert!(matches!(err, InventoryError::NotFound));
        }

        #[test]
        fn remove_unit_on_invisible_resource_returns_not_found() {
            let mut f = other_project();
            let err = f
                .store
                .tx(|tx| {
                    f.inv
                        .remove_unit(tx, &caller(Some("/p/a")), &rid("b"), &uid("u1"))
                })
                .unwrap_err();
            assert!(matches!(err, InventoryError::NotFound));
        }

        #[test]
        fn retire_unit_on_invisible_resource_returns_not_found() {
            let mut f = other_project();
            let err = f
                .store
                .tx(|tx| f.inv.retire_unit(tx, &caller(None), &rid("b"), &uid("u1")))
                .unwrap_err();
            assert!(matches!(err, InventoryError::NotFound));
        }

        #[test]
        fn failed_mutations_write_no_audit_row() {
            let mut f = other_project();
            let before = f.audit_actions().len();
            let who = caller(Some("/p/a"));
            let _ = f.store.tx(|tx| f.inv.delete_resource(tx, &who, &rid("b")));
            let _ = f
                .store
                .tx(|tx| f.inv.remove_unit(tx, &who, &rid("b"), &uid("u1")));
            let _ = f
                .store
                .tx(|tx| f.inv.add_unit(tx, &who, &rid("b"), &new_unit("u2", "/x")));
            let _ = f.store.tx(|tx| {
                f.inv
                    .create_resource(tx, &who, &new_device("b", Scope::Project(project("/p/b"))))
            });
            assert_eq!(f.audit_actions().len(), before);
        }
    }
}

fn new_generic(id: &str, scope: Scope) -> NewResource {
    NewResource {
        ty: ResourceType::Generic,
        ..new_device(id, scope)
    }
}

fn generic_settings(key: &str) -> UnitSettings {
    UnitSettings::Generic {
        identity_key: key.into(),
    }
}

fn generic_unit(id: &str, key: &str) -> NewUnit {
    NewUnit {
        id: uid(id),
        settings: generic_settings(key),
        description: String::new(),
    }
}

fn scope_of(path: Option<&str>) -> Scope {
    path.map_or(Scope::User, |p| Scope::Project(project(p)))
}

impl Fixture {
    /// Create a generic resource in `scope` (as a caller of that scope) with
    /// one unit keyed `key`; returns the add-unit result.
    fn seed_generic(
        &mut self,
        scope_path: Option<&str>,
        id: &str,
        unit: &str,
        key: &str,
    ) -> Result<leclerk::inventory::Created<leclerk::model::Unit>, InventoryError> {
        let who = caller(scope_path);
        self.store.tx(|tx| {
            self.inv
                .create_resource(tx, &who, &new_generic(id, scope_of(scope_path)))?;
            self.inv
                .add_unit(tx, &who, &rid(id), &generic_unit(unit, key))
        })
    }

    fn row(&mut self, scope_path: Option<&str>, res: &str, unit: &str) -> i64 {
        let who = caller(scope_path);
        let r = self
            .store
            .tx(|tx| self.inv.get_resource(tx, &who, &rid(res)))
            .unwrap();
        r.units.iter().find(|u| u.id == uid(unit)).unwrap().row
    }

    fn group(&mut self, row: i64) -> Vec<i64> {
        self.store
            .tx(|tx| Ok::<_, InventoryError>(Inventory::identity_group(tx, row).unwrap()))
            .unwrap()
    }

    fn pending(&mut self) -> Vec<leclerk::inventory::Promotion> {
        self.store.tx(|tx| self.promos.list(tx)).unwrap()
    }

    fn identity_text(&mut self, row: i64) -> Option<String> {
        self.store
            .tx(|tx| {
                Ok::<_, InventoryError>(
                    tx.conn()
                        .query_row("SELECT identity FROM units WHERE row_id = ?1", [row], |r| {
                            r.get(0)
                        })
                        .unwrap(),
                )
            })
            .unwrap()
    }
}

mod duplicate_handling {
    use super::*;

    mod add_unit {
        use super::*;

        mod on_match_in_same_scope {
            use super::*;

            #[test]
            fn returns_duplicate_in_scope_naming_the_existing_unit() {
                let mut f = fixture();
                f.seed_generic(None, "a", "u1", "k").unwrap();
                let who = caller(None);
                let err = f
                    .store
                    .tx(|tx| {
                        f.inv
                            .create_resource(tx, &who, &new_generic("b", Scope::User))?;
                        f.inv
                            .add_unit(tx, &who, &rid("b"), &generic_unit("u9", "k"))
                    })
                    .unwrap_err();
                match err {
                    InventoryError::DuplicateInScope { resource, unit, .. } => {
                        assert_eq!(resource, rid("a"));
                        assert_eq!(unit, uid("u1"));
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }

            #[test]
            fn persists_nothing_and_writes_no_audit() {
                let mut f = fixture();
                f.seed_generic(None, "a", "u1", "k").unwrap();
                let who = caller(None);
                f.store
                    .tx(|tx| {
                        f.inv
                            .create_resource(tx, &who, &new_generic("b", Scope::User))
                    })
                    .unwrap();
                let before = f.audit_actions().len();
                let _ = f.store.tx(|tx| {
                    f.inv
                        .add_unit(tx, &who, &rid("b"), &generic_unit("u9", "k"))
                });
                let b = f
                    .store
                    .tx(|tx| f.inv.get_resource(tx, &who, &rid("b")))
                    .unwrap();
                assert_eq!(b.units.len(), 0);
                assert_eq!(f.audit_actions().len(), before);
            }

            #[test]
            fn applies_within_one_project_too() {
                let mut f = fixture();
                f.seed_generic(Some("/p/a"), "a", "u1", "k").unwrap();
                let who = caller(Some("/p/a"));
                let err = f
                    .store
                    .tx(|tx| {
                        f.inv.create_resource(
                            tx,
                            &who,
                            &new_generic("b", Scope::Project(project("/p/a"))),
                        )?;
                        f.inv
                            .add_unit(tx, &who, &rid("b"), &generic_unit("u9", "k"))
                    })
                    .unwrap_err();
                assert!(matches!(err, InventoryError::DuplicateInScope { .. }));
            }
        }

        mod on_hits_in_the_same_scope_and_another_scope {
            use super::*;

            /// Counts of (links, promotions, audit rows).
            fn state(f: &mut Fixture) -> (i64, usize, usize) {
                let links = f
                    .store
                    .tx(|tx| {
                        Ok::<_, InventoryError>(
                            tx.conn()
                                .query_row("SELECT count(*) FROM unit_links", [], |r| r.get(0))
                                .unwrap(),
                        )
                    })
                    .unwrap();
                (links, f.pending().len(), f.audit_actions().len())
            }

            fn rejects_without_trace(other_scope: Option<&str>) {
                let mut f = fixture();
                f.seed_generic(Some("/p/a"), "mine", "m1", "k").unwrap();
                f.seed_generic(other_scope, "elsewhere", "e1", "k").unwrap();
                let before = state(&mut f);
                let who = caller(Some("/p/a"));
                let err = f
                    .store
                    .tx(|tx| {
                        f.inv.create_resource(
                            tx,
                            &who,
                            &new_generic("again", Scope::Project(project("/p/a"))),
                        )?;
                        f.inv
                            .add_unit(tx, &who, &rid("again"), &generic_unit("a1", "k"))
                    })
                    .unwrap_err();
                assert!(matches!(
                    err,
                    InventoryError::DuplicateInScope { ref resource, ref unit, .. }
                        if *resource == rid("mine") && *unit == uid("m1")
                ));
                assert_eq!(state(&mut f), before);
            }

            #[test]
            fn returns_duplicate_in_scope_without_link_promotion_or_audit_when_other_is_a_project()
            {
                rejects_without_trace(Some("/p/b"));
            }

            #[test]
            fn returns_duplicate_in_scope_without_link_promotion_or_audit_when_other_is_user() {
                rejects_without_trace(None);
            }
        }

        mod on_no_match {
            use super::*;

            #[test]
            fn succeeds_without_warnings_links_or_promotions() {
                let mut f = fixture();
                f.seed_generic(None, "a", "u1", "k1").unwrap();
                let created = f.seed_generic(Some("/p/a"), "b", "u1", "k2").unwrap();
                assert_eq!(created.warnings.len(), 0);
                let row = f.row(Some("/p/a"), "b", "u1");
                assert_eq!(f.group(row), vec![row]);
                assert_eq!(f.pending().len(), 0);
            }

            #[test]
            fn stores_the_extracted_identity() {
                let mut f = fixture();
                f.seed_generic(None, "a", "u1", "k1").unwrap();
                let row = f.row(None, "a", "u1");
                let text = f.identity_text(row).unwrap();
                assert!(text.contains("\"generic\""), "{text}");
                assert!(text.contains("k1"), "{text}");
            }

            #[test]
            fn does_not_compare_across_resource_types() {
                let mut f = fixture();
                f.seed_generic(None, "a", "u1", "/dev/ttyUSB0").unwrap();
                let who = caller(None);
                let created = f
                    .store
                    .tx(|tx| {
                        f.inv
                            .create_resource(tx, &who, &new_device("d", Scope::User))?;
                        f.inv
                            .add_unit(tx, &who, &rid("d"), &new_unit("u1", "/dev/ttyUSB0"))
                    })
                    .unwrap();
                assert_eq!(created.warnings.len(), 0);
            }
        }

        mod on_project_unit_matching_a_user_unit {
            use super::*;

            #[test]
            fn succeeds_with_duplicate_of_user_unit_warning() {
                let mut f = fixture();
                f.seed_generic(None, "board", "u1", "k").unwrap();
                let created = f.seed_generic(Some("/p/a"), "mine", "m1", "k").unwrap();
                assert_eq!(
                    created.warnings,
                    vec![Warning::DuplicateOfUserUnit {
                        resource: rid("board"),
                        unit: uid("u1"),
                        resource_description: "a board".into(),
                        unit_description: String::new(),
                    }]
                );
            }

            #[test]
            fn links_the_two_units_into_one_identity_group() {
                let mut f = fixture();
                f.seed_generic(None, "board", "u1", "k").unwrap();
                f.seed_generic(Some("/p/a"), "mine", "m1", "k").unwrap();
                let a = f.row(None, "board", "u1");
                let b = f.row(Some("/p/a"), "mine", "m1");
                let mut want = vec![a, b];
                want.sort_unstable();
                assert_eq!(f.group(a), want);
                assert_eq!(f.group(b), want);
            }

            #[test]
            fn stores_the_link_once() {
                let mut f = fixture();
                f.seed_generic(None, "board", "u1", "k").unwrap();
                f.seed_generic(Some("/p/a"), "mine", "m1", "k").unwrap();
                let n: i64 = f
                    .store
                    .tx(|tx| {
                        Ok::<_, InventoryError>(
                            tx.conn()
                                .query_row("SELECT count(*) FROM unit_links", [], |r| r.get(0))
                                .unwrap(),
                        )
                    })
                    .unwrap();
                assert_eq!(n, 1);
            }

            #[test]
            fn creates_no_promotion() {
                let mut f = fixture();
                f.seed_generic(None, "board", "u1", "k").unwrap();
                f.seed_generic(Some("/p/a"), "mine", "m1", "k").unwrap();
                assert_eq!(f.pending().len(), 0);
            }
        }

        mod on_match_in_another_projects_scope {
            use super::*;

            fn two_projects() -> (Fixture, leclerk::inventory::Created<leclerk::model::Unit>) {
                let mut f = fixture();
                f.seed_generic(Some("/p/b"), "secret-res", "secret-unit", "k")
                    .unwrap();
                let created = f.seed_generic(Some("/p/a"), "mine", "m1", "k").unwrap();
                (f, created)
            }

            #[test]
            fn succeeds_with_similar_in_other_project_warning() {
                let (mut f, created) = two_projects();
                let promos = f.pending();
                assert_eq!(promos.len(), 1);
                assert_eq!(
                    created.warnings,
                    vec![Warning::SimilarInOtherProject {
                        project: project("/p/b"),
                        promotion_id: promos[0].id.clone(),
                    }]
                );
            }

            #[test]
            fn creates_a_pending_promotion_of_the_other_resource() {
                let (mut f, _) = two_projects();
                let promos = f.pending();
                assert_eq!(promos[0].resource, rid("secret-res"));
                assert_eq!(promos[0].requester, Some(project("/p/a")));
                assert_ne!(promos[0].reason, "");
            }

            #[test]
            fn links_the_two_units() {
                let (mut f, _) = two_projects();
                let a = f.row(Some("/p/a"), "mine", "m1");
                let b = f.row(Some("/p/b"), "secret-res", "secret-unit");
                assert!(f.group(a).contains(&b));
            }

            #[test]
            fn discloses_only_the_other_projects_path() {
                let (mut f, created) = two_projects();
                // Give the other project's side recognisable description/settings.
                let b = caller(Some("/p/b"));
                f.store
                    .tx(|tx| {
                        f.inv.update_unit(
                            tx,
                            &b,
                            &rid("secret-res"),
                            &uid("secret-unit"),
                            UnitPatch {
                                settings: None,
                                description: Some("hunter2-notes".into()),
                            },
                        )
                    })
                    .unwrap();
                let json = serde_json::to_string(&created.warnings).unwrap();
                assert!(json.contains("/p/b"), "{json}");
                for leak in ["secret-res", "secret-unit", "hunter2", "\"k\"", "a board"] {
                    assert!(!json.contains(leak), "{leak} leaked in {json}");
                }
            }

            #[test]
            fn reuses_the_id_when_the_same_project_matches_again_via_update() {
                let (mut f, first) = two_projects();
                let a = caller(Some("/p/a"));
                f.store
                    .tx(|tx| {
                        f.inv.update_unit(
                            tx,
                            &a,
                            &rid("mine"),
                            &uid("m1"),
                            UnitPatch {
                                settings: Some(generic_settings("other")),
                                description: None,
                            },
                        )?;
                        f.inv.update_unit(
                            tx,
                            &a,
                            &rid("mine"),
                            &uid("m1"),
                            UnitPatch {
                                settings: Some(generic_settings("k")),
                                description: None,
                            },
                        )
                    })
                    .map(|c| {
                        assert_eq!(c.warnings, first.warnings);
                    })
                    .unwrap();
                assert_eq!(f.pending().len(), 1);
            }
        }

        mod on_user_unit_matching_a_project_unit {
            use super::*;

            #[test]
            fn warns_duplicate_of_project_unit_when_the_caller_sees_that_project() {
                let mut f = fixture();
                f.seed_generic(Some("/p/a"), "mine", "m1", "k").unwrap();
                // Caller in /p/a registers at user level.
                let who = caller(Some("/p/a"));
                let created = f
                    .store
                    .tx(|tx| {
                        f.inv
                            .create_resource(tx, &who, &new_generic("board", Scope::User))?;
                        f.inv
                            .add_unit(tx, &who, &rid("board"), &generic_unit("u1", "k"))
                    })
                    .unwrap();
                assert_eq!(
                    created.warnings,
                    vec![Warning::DuplicateOfProjectUnit {
                        resource: rid("mine"),
                        unit: uid("m1"),
                        resource_description: "a board".into(),
                        unit_description: String::new(),
                    }]
                );
                let a = f.row(None, "board", "u1");
                let b = f.row(Some("/p/a"), "mine", "m1");
                assert!(f.group(a).contains(&b));
            }

            #[test]
            fn warns_similar_in_other_project_when_the_caller_cannot_see_it() {
                let mut f = fixture();
                f.seed_generic(Some("/p/b"), "secret-res", "secret-unit", "k")
                    .unwrap();
                let created = f.seed_generic(None, "board", "u1", "k").unwrap();
                let promos = f.pending();
                assert_eq!(promos.len(), 1);
                assert_eq!(
                    created.warnings,
                    vec![Warning::SimilarInOtherProject {
                        project: project("/p/b"),
                        promotion_id: promos[0].id.clone(),
                    }]
                );
                let a = f.row(None, "board", "u1");
                let b = f.row(Some("/p/b"), "secret-res", "secret-unit");
                assert!(f.group(a).contains(&b));
            }
        }
    }

    mod update_unit {
        use super::*;

        fn patch_key(key: &str) -> UnitPatch {
            UnitPatch {
                settings: Some(generic_settings(key)),
                description: None,
            }
        }

        #[test]
        fn on_new_settings_matching_same_scope_returns_duplicate_and_changes_nothing() {
            let mut f = fixture();
            f.seed_generic(None, "a", "u1", "k1").unwrap();
            f.seed_generic(None, "b", "u1", "k2").unwrap();
            let who = caller(None);
            let before = f.audit_actions().len();
            let err = f
                .store
                .tx(|tx| {
                    f.inv
                        .update_unit(tx, &who, &rid("b"), &uid("u1"), patch_key("k1"))
                })
                .unwrap_err();
            assert!(matches!(
                err,
                InventoryError::DuplicateInScope { ref resource, ref unit, .. }
                    if *resource == rid("a") && *unit == uid("u1")
            ));
            let b = f
                .store
                .tx(|tx| f.inv.get_resource(tx, &who, &rid("b")))
                .unwrap();
            assert_eq!(b.units[0].settings, generic_settings("k2"));
            assert_eq!(f.audit_actions().len(), before);
        }

        #[test]
        fn on_unchanged_settings_does_not_collide_with_itself() {
            let mut f = fixture();
            f.seed_generic(None, "a", "u1", "k1").unwrap();
            let who = caller(None);
            let c = f
                .store
                .tx(|tx| {
                    f.inv
                        .update_unit(tx, &who, &rid("a"), &uid("u1"), patch_key("k1"))
                })
                .unwrap();
            assert_eq!(c.warnings.len(), 0);
        }

        #[test]
        fn on_new_settings_matching_a_user_unit_warns_and_links() {
            let mut f = fixture();
            f.seed_generic(None, "board", "u1", "k").unwrap();
            f.seed_generic(Some("/p/a"), "mine", "m1", "other").unwrap();
            let who = caller(Some("/p/a"));
            let c = f
                .store
                .tx(|tx| {
                    f.inv
                        .update_unit(tx, &who, &rid("mine"), &uid("m1"), patch_key("k"))
                })
                .unwrap();
            assert_eq!(
                c.warnings,
                vec![Warning::DuplicateOfUserUnit {
                    resource: rid("board"),
                    unit: uid("u1"),
                    resource_description: "a board".into(),
                    unit_description: String::new(),
                }]
            );
            let a = f.row(None, "board", "u1");
            let b = f.row(Some("/p/a"), "mine", "m1");
            assert!(f.group(a).contains(&b));
        }

        #[test]
        fn on_new_settings_matching_another_project_creates_a_promotion() {
            let mut f = fixture();
            f.seed_generic(Some("/p/b"), "theirs", "t1", "k").unwrap();
            f.seed_generic(Some("/p/a"), "mine", "m1", "other").unwrap();
            let who = caller(Some("/p/a"));
            let c = f
                .store
                .tx(|tx| {
                    f.inv
                        .update_unit(tx, &who, &rid("mine"), &uid("m1"), patch_key("k"))
                })
                .unwrap();
            assert!(matches!(
                c.warnings[0],
                Warning::SimilarInOtherProject { .. }
            ));
            assert_eq!(f.pending().len(), 1);
        }

        #[test]
        fn on_settings_moving_away_drops_the_automatic_link() {
            let mut f = fixture();
            f.seed_generic(None, "board", "u1", "k").unwrap();
            f.seed_generic(Some("/p/a"), "mine", "m1", "k").unwrap();
            let who = caller(Some("/p/a"));
            let a = f.row(None, "board", "u1");
            let b = f.row(Some("/p/a"), "mine", "m1");
            let c = f
                .store
                .tx(|tx| {
                    f.inv
                        .update_unit(tx, &who, &rid("mine"), &uid("m1"), patch_key("zzz"))
                })
                .unwrap();
            assert_eq!(c.warnings.len(), 0);
            assert_eq!(f.group(a), vec![a]);
            assert_eq!(f.group(b), vec![b]);
        }

        #[test]
        fn on_settings_change_keeps_manual_links() {
            let mut f = fixture();
            f.seed_generic(None, "board", "u1", "k1").unwrap();
            f.seed_generic(Some("/p/a"), "mine", "m1", "k2").unwrap();
            let a = f.row(None, "board", "u1");
            let b = f.row(Some("/p/a"), "mine", "m1");
            f.store.tx(|tx| Inventory::link(tx, a, b)).unwrap();
            let who = caller(Some("/p/a"));
            f.store
                .tx(|tx| {
                    f.inv
                        .update_unit(tx, &who, &rid("mine"), &uid("m1"), patch_key("k3"))
                })
                .unwrap();
            assert!(f.group(a).contains(&b));
        }

        #[test]
        fn on_settings_change_recomputes_the_stored_identity() {
            let mut f = fixture();
            f.seed_generic(None, "a", "u1", "k1").unwrap();
            let who = caller(None);
            f.store
                .tx(|tx| {
                    f.inv
                        .update_unit(tx, &who, &rid("a"), &uid("u1"), patch_key("k2"))
                })
                .unwrap();
            let row = f.row(None, "a", "u1");
            assert!(f.identity_text(row).unwrap().contains("k2"));
        }

        #[test]
        fn on_description_only_change_keeps_links_and_returns_no_warnings() {
            let mut f = fixture();
            f.seed_generic(None, "board", "u1", "k").unwrap();
            f.seed_generic(Some("/p/a"), "mine", "m1", "k").unwrap();
            let who = caller(Some("/p/a"));
            let c = f
                .store
                .tx(|tx| {
                    f.inv.update_unit(
                        tx,
                        &who,
                        &rid("mine"),
                        &uid("m1"),
                        UnitPatch {
                            settings: None,
                            description: Some("n".into()),
                        },
                    )
                })
                .unwrap();
            assert_eq!(c.warnings.len(), 0);
            let a = f.row(None, "board", "u1");
            let b = f.row(Some("/p/a"), "mine", "m1");
            assert!(f.group(a).contains(&b));
        }
    }

    mod identity_group {
        use super::*;

        #[test]
        fn is_the_transitive_closure_of_links_including_the_unit() {
            let mut f = fixture();
            f.seed_generic(None, "x", "u", "k1").unwrap();
            f.seed_generic(None, "y", "u", "k2").unwrap();
            f.seed_generic(None, "z", "u", "k3").unwrap();
            f.seed_generic(None, "w", "u", "k4").unwrap();
            let (ux, uy, uz, uw) = (
                f.row(None, "x", "u"),
                f.row(None, "y", "u"),
                f.row(None, "z", "u"),
                f.row(None, "w", "u"),
            );
            f.store
                .tx(|tx| {
                    Inventory::link(tx, ux, uy)?;
                    Inventory::link(tx, uz, uy)
                })
                .unwrap();
            let mut want = vec![ux, uy, uz];
            want.sort_unstable();
            assert_eq!(f.group(ux), want);
            assert_eq!(f.group(uz), want);
            assert_eq!(f.group(uw), vec![uw]);
        }

        #[test]
        fn terminates_on_a_cycle_of_links() {
            let mut f = fixture();
            f.seed_generic(None, "x", "u", "k1").unwrap();
            f.seed_generic(None, "y", "u", "k2").unwrap();
            f.seed_generic(None, "z", "u", "k3").unwrap();
            let (ux, uy, uz) = (
                f.row(None, "x", "u"),
                f.row(None, "y", "u"),
                f.row(None, "z", "u"),
            );
            f.store
                .tx(|tx| {
                    Inventory::link(tx, ux, uy)?;
                    Inventory::link(tx, uy, uz)?;
                    Inventory::link(tx, uz, ux)
                })
                .unwrap();
            let mut want = vec![ux, uy, uz];
            want.sort_unstable();
            assert_eq!(f.group(uy), want);
        }

        #[test]
        fn unlink_splits_the_group() {
            let mut f = fixture();
            f.seed_generic(None, "x", "u", "k1").unwrap();
            f.seed_generic(None, "y", "u", "k2").unwrap();
            let (x, y) = (f.row(None, "x", "u"), f.row(None, "y", "u"));
            f.store
                .tx(|tx| {
                    Inventory::link(tx, x, y)?;
                    Inventory::unlink(tx, y, x)
                })
                .unwrap();
            assert_eq!(f.group(x), vec![x]);
        }

        #[test]
        fn linking_twice_stores_one_link() {
            let mut f = fixture();
            f.seed_generic(None, "x", "u", "k1").unwrap();
            f.seed_generic(None, "y", "u", "k2").unwrap();
            let (x, y) = (f.row(None, "x", "u"), f.row(None, "y", "u"));
            f.store
                .tx(|tx| {
                    Inventory::link(tx, x, y)?;
                    Inventory::link(tx, y, x)
                })
                .unwrap();
            let n: i64 = f
                .store
                .tx(|tx| {
                    Ok::<_, InventoryError>(
                        tx.conn()
                            .query_row("SELECT count(*) FROM unit_links", [], |r| r.get(0))
                            .unwrap(),
                    )
                })
                .unwrap();
            assert_eq!(n, 1);
        }
    }
}

mod update_unit_exclude_self {
    use super::*;

    // A real USB serial needs a device node (mknod, root). A symlink to the
    // same file reaches the same code path: the new identity matches the
    // unit's own old one (canonical path and inode), which must not count
    // as a duplicate.
    #[test]
    fn new_settings_matching_the_units_own_old_identity_do_not_collide() {
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("tty0");
        std::fs::write(&real, "").unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let sysfs = dir.path().join("sys");
        std::fs::create_dir(&sysfs).unwrap();

        let clock = Arc::new(ManualClock::new(1));
        let inv = Inventory::with_sysfs(clock, sysfs);
        let mut store = Store::open_in_memory().unwrap();
        let who = caller(None);
        let patch = UnitPatch {
            settings: Some(device_settings(alias.to_str().unwrap())),
            description: None,
        };
        let created = store
            .tx(|tx| {
                inv.create_resource(tx, &who, &new_device("d", Scope::User))?;
                inv.add_unit(tx, &who, &rid("d"), &new_unit("u1", real.to_str().unwrap()))?;
                inv.update_unit(tx, &who, &rid("d"), &uid("u1"), patch)
            })
            .unwrap();
        assert_eq!(created.warnings.len(), 0);
        assert_eq!(
            created.value.settings,
            device_settings(alias.to_str().unwrap())
        );
    }
}

mod promotions {
    use super::*;

    /// Project b has `theirs`; project a registered a match, so a promotion
    /// of `theirs` is pending.
    fn pending_promotion() -> (Fixture, leclerk::model::PromotionId) {
        let mut f = fixture();
        f.seed_generic(Some("/p/b"), "theirs", "t1", "k").unwrap();
        f.seed_generic(Some("/p/a"), "mine", "m1", "k").unwrap();
        let id = f.pending()[0].id.clone();
        (f, id)
    }

    fn admin() -> Caller {
        Caller {
            override_reason: Some("share the board".into()),
            ..caller(None)
        }
    }

    mod list {
        use super::*;

        #[test]
        fn returns_pending_promotions_with_requester_and_reason() {
            let (mut f, id) = pending_promotion();
            let list = f.pending();
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].id, id);
            assert_eq!(list[0].resource, rid("theirs"));
            assert_eq!(list[0].requester, Some(project("/p/a")));
            assert_eq!(list[0].reason, "a matching unit was registered there");
        }
    }

    mod approve {
        use super::*;

        mod on_no_id_clash {
            use super::*;

            #[test]
            fn moves_the_resource_to_user_scope_keeping_its_id() {
                let (mut f, id) = pending_promotion();
                let r = f
                    .store
                    .tx(|tx| f.promos.approve(tx, &admin(), id.clone(), None))
                    .unwrap();
                assert_eq!(r.id, rid("theirs"));
                assert_eq!(r.scope, Scope::User);
                let seen = f
                    .store
                    .tx(|tx| f.inv.get_resource(tx, &caller(None), &rid("theirs")))
                    .unwrap();
                assert_eq!(seen.scope, Scope::User);
            }

            #[test]
            fn clears_the_promotion() {
                let (mut f, id) = pending_promotion();
                f.store
                    .tx(|tx| f.promos.approve(tx, &admin(), id.clone(), None))
                    .unwrap();
                assert_eq!(f.pending().len(), 0);
                let again = f
                    .store
                    .tx(|tx| f.promos.approve(tx, &admin(), id.clone(), None))
                    .unwrap_err();
                assert!(matches!(again, PromotionError::NotFound));
            }

            #[test]
            fn keeps_existing_links() {
                let (mut f, id) = pending_promotion();
                f.store
                    .tx(|tx| f.promos.approve(tx, &admin(), id, None))
                    .unwrap();
                let a = f.row(Some("/p/a"), "mine", "m1");
                let b = f.row(None, "theirs", "t1");
                assert!(f.group(a).contains(&b));
            }

            #[test]
            fn writes_audit_entry_with_the_user_request() {
                let (mut f, id) = pending_promotion();
                f.store
                    .tx(|tx| f.promos.approve(tx, &admin(), id, None))
                    .unwrap();
                let entries = f.store.tx(audit::all).unwrap();
                let last = entries.last().unwrap();
                assert_eq!(last.action, "promotion_approved");
                assert_eq!(last.override_reason.as_deref(), Some("share the board"));
            }
        }

        mod on_id_clash_with_a_user_resource_and_no_new_id {
            use super::*;

            fn clashing() -> (Fixture, leclerk::model::PromotionId) {
                let (mut f, id) = pending_promotion();
                f.seed_generic(None, "theirs", "x", "unrelated").unwrap();
                (f, id)
            }

            #[test]
            fn returns_id_clash() {
                let (mut f, id) = clashing();
                let err = f
                    .store
                    .tx(|tx| f.promos.approve(tx, &admin(), id, None))
                    .unwrap_err();
                assert!(matches!(err, PromotionError::IdClash { ref id } if *id == rid("theirs")));
            }

            #[test]
            fn leaves_the_promotion_pending_and_the_resource_in_place() {
                let (mut f, id) = clashing();
                let _ = f.store.tx(|tx| f.promos.approve(tx, &admin(), id, None));
                assert_eq!(f.pending().len(), 1);
                let r = f
                    .store
                    .tx(|tx| {
                        f.inv
                            .get_resource(tx, &caller(Some("/p/b")), &rid("theirs"))
                    })
                    .unwrap();
                assert_eq!(r.scope, Scope::Project(project("/p/b")));
            }
        }

        mod on_id_clash_with_new_id_given {
            use super::*;

            #[test]
            fn moves_and_renames() {
                let (mut f, id) = pending_promotion();
                f.seed_generic(None, "theirs", "x", "unrelated").unwrap();
                let r = f
                    .store
                    .tx(|tx| f.promos.approve(tx, &admin(), id, Some(rid("shared"))))
                    .unwrap();
                assert_eq!(r.id, rid("shared"));
                assert_eq!(r.scope, Scope::User);
                assert_eq!(r.units[0].id, uid("t1"));
            }

            #[test]
            fn still_returns_id_clash_when_the_new_id_is_also_taken() {
                let (mut f, id) = pending_promotion();
                f.seed_generic(None, "theirs", "x", "unrelated").unwrap();
                f.seed_generic(None, "shared", "x", "unrelated2").unwrap();
                let err = f
                    .store
                    .tx(|tx| f.promos.approve(tx, &admin(), id, Some(rid("shared"))))
                    .unwrap_err();
                assert!(matches!(err, PromotionError::IdClash { ref id } if *id == rid("shared")));
            }
        }

        #[test]
        fn on_unknown_promotion_returns_not_found() {
            let mut f = fixture();
            let err = f
                .store
                .tx(|tx| {
                    f.promos
                        .approve(tx, &admin(), leclerk::model::PromotionId::new(), None)
                })
                .unwrap_err();
            assert!(matches!(err, PromotionError::NotFound));
        }
    }
}

mod dedup_audit {
    use super::*;

    fn entries(f: &mut Fixture, action: &str) -> Vec<audit::AuditEntry> {
        f.store
            .tx(audit::all)
            .unwrap()
            .into_iter()
            .filter(|e| e.action == action)
            .collect()
    }

    mod add_unit {
        use super::*;

        mod on_a_match_in_another_project {
            use super::*;

            #[test]
            fn audits_promotion_requested() {
                let mut f = fixture();
                f.seed_generic(Some("/p/b"), "theirs", "t1", "k").unwrap();
                f.seed_generic(Some("/p/a"), "mine", "m1", "k").unwrap();
                let promos = f.pending();
                let got = entries(&mut f, "promotion_requested");
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].detail["promotion"], promos[0].id.to_string());
                assert_eq!(got[0].detail["resource"], "theirs");
            }
        }

        mod on_an_automatic_link {
            use super::*;

            #[test]
            fn audits_link_created_listing_the_pairs() {
                let mut f = fixture();
                f.seed_generic(None, "board", "u1", "k").unwrap();
                f.seed_generic(Some("/p/a"), "mine", "m1", "k").unwrap();
                let got = entries(&mut f, "link_created");
                assert_eq!(got.len(), 1);
                assert_eq!(
                    got[0].detail["links"],
                    serde_json::json!([["mine/m1", "board/u1"]])
                );
            }

            #[test]
            fn audits_nothing_without_a_match() {
                let mut f = fixture();
                f.seed_generic(None, "board", "u1", "k").unwrap();
                f.seed_generic(None, "other", "u1", "k2").unwrap();
                assert_eq!(entries(&mut f, "link_created").len(), 0);
            }
        }
    }
}

mod text_fields {
    use leclerk::error::ErrorCode;
    use leclerk::model::TextError;

    use super::*;

    fn long_description() -> String {
        "x".repeat(201)
    }

    fn long_usage() -> String {
        "u".repeat(4097)
    }

    fn resource_count(f: &mut Fixture) -> usize {
        let who = caller(None);
        f.store
            .tx(|tx| f.inv.list_resources(tx, &who, View::All, &[]))
            .unwrap()
            .len()
    }

    fn get(f: &mut Fixture, who: &Caller, id: &str) -> Resource {
        f.store
            .tx(|tx| f.inv.get_resource(tx, who, &rid(id)))
            .unwrap()
    }

    mod create_resource {
        use super::*;

        mod on_valid_description_and_usage {
            use super::*;

            fn create(f: &mut Fixture) -> Resource {
                let who = caller(None);
                let mut n = new_device("board", Scope::User);
                n.description = "a dev board".into();
                n.usage = "Flash with `esptool --port {unit.path}` ${PORT} {}".into();
                f.store
                    .tx(|tx| f.inv.create_resource(tx, &who, &n))
                    .unwrap();
                get(f, &who, "board")
            }

            #[test]
            fn stores_the_description() {
                let mut f = fixture();
                assert_eq!(create(&mut f).description, "a dev board");
            }

            #[test]
            fn get_returns_the_raw_template() {
                let mut f = fixture();
                assert_eq!(
                    create(&mut f).usage,
                    "Flash with `esptool --port {unit.path}` ${PORT} {}"
                );
            }
        }

        mod on_invalid_description_or_usage {
            use super::*;

            fn refused(f: &mut Fixture, tweak: impl FnOnce(&mut NewResource)) -> InventoryError {
                let who = caller(None);
                let mut n = new_device("board", Scope::User);
                tweak(&mut n);
                f.store
                    .tx(|tx| f.inv.create_resource(tx, &who, &n))
                    .unwrap_err()
            }

            #[test]
            fn returns_invalid_text_for_a_too_long_description() {
                let mut f = fixture();
                let e = refused(&mut f, |n| n.description = long_description());
                assert!(matches!(
                    e,
                    InventoryError::InvalidText(TextError::DescriptionTooLong { max: 200 })
                ));
                assert_eq!(e.code(), "description_too_long");
            }

            #[test]
            fn returns_invalid_text_for_a_multiline_description() {
                let mut f = fixture();
                let e = refused(&mut f, |n| n.description = "a\nb".into());
                assert!(matches!(
                    e,
                    InventoryError::InvalidText(TextError::DescriptionMultiline)
                ));
                assert_eq!(e.code(), "description_multiline");
            }

            #[test]
            fn returns_invalid_text_for_a_too_long_usage() {
                let mut f = fixture();
                let e = refused(&mut f, |n| n.usage = long_usage());
                assert!(matches!(
                    e,
                    InventoryError::InvalidText(TextError::UsageTooLong { max: 4096 })
                ));
                assert_eq!(e.code(), "usage_too_long");
            }

            #[test]
            fn stores_nothing() {
                let mut f = fixture();
                refused(&mut f, |n| n.usage = long_usage());
                assert_eq!(resource_count(&mut f), 0);
            }

            #[test]
            fn writes_no_audit_entry() {
                let mut f = fixture();
                refused(&mut f, |n| n.description = "a\nb".into());
                assert_eq!(f.audit_actions(), Vec::<String>::new());
            }
        }

        mod on_unknown_placeholder {
            use super::*;

            fn refused(f: &mut Fixture) -> InventoryError {
                let who = caller(None);
                let mut n = new_device("board", Scope::User);
                n.usage = "open {unit.pth}".into();
                f.store
                    .tx(|tx| f.inv.create_resource(tx, &who, &n))
                    .unwrap_err()
            }

            #[test]
            fn is_refused_with_code_unknown_placeholder() {
                let mut f = fixture();
                assert_eq!(refused(&mut f).code(), "unknown_placeholder");
            }

            #[test]
            fn details_name_the_placeholder_and_the_valid_keys() {
                let mut f = fixture();
                let details = refused(&mut f).details().unwrap();
                assert_eq!(details["placeholder"], "unit.pth");
                assert!(
                    details["valid"]
                        .as_array()
                        .unwrap()
                        .contains(&serde_json::json!("unit.path"))
                );
            }

            #[test]
            fn refuses_a_key_of_another_type() {
                let mut f = fixture();
                let who = caller(None);
                let mut n = new_device("board", Scope::User);
                n.usage = "{unit.port}".into();
                let e = f
                    .store
                    .tx(|tx| f.inv.create_resource(tx, &who, &n))
                    .unwrap_err();
                assert_eq!(e.code(), "unknown_placeholder");
            }
        }
    }

    mod update_resource {
        use super::*;

        fn seeded() -> (Fixture, Caller) {
            let mut f = fixture();
            let who = caller(None);
            let mut n = new_device("board", Scope::User);
            n.usage = "old {unit.path}".into();
            f.store
                .tx(|tx| f.inv.create_resource(tx, &who, &n))
                .unwrap();
            (f, who)
        }

        fn patch(
            f: &mut Fixture,
            who: &Caller,
            p: ResourcePatch,
        ) -> Result<Resource, InventoryError> {
            f.store
                .tx(|tx| f.inv.update_resource(tx, who, &rid("board"), p))
        }

        mod on_usage_patch {
            use super::*;

            #[test]
            fn stores_a_template_valid_for_the_stored_type() {
                let (mut f, who) = seeded();
                let updated = patch(
                    &mut f,
                    &who,
                    ResourcePatch {
                        usage: Some("new {unit.path} {resource.id}".into()),
                        ..ResourcePatch::default()
                    },
                )
                .unwrap();
                assert_eq!(updated.usage, "new {unit.path} {resource.id}");
            }

            #[test]
            fn refuses_a_key_invalid_for_the_stored_type() {
                let (mut f, who) = seeded();
                let e = patch(
                    &mut f,
                    &who,
                    ResourcePatch {
                        usage: Some("{unit.port}".into()),
                        ..ResourcePatch::default()
                    },
                )
                .unwrap_err();
                assert_eq!(e.code(), "unknown_placeholder");
            }

            #[test]
            fn keeps_the_old_usage_when_refused() {
                let (mut f, who) = seeded();
                let _ = patch(
                    &mut f,
                    &who,
                    ResourcePatch {
                        usage: Some(long_usage()),
                        ..ResourcePatch::default()
                    },
                );
                assert_eq!(get(&mut f, &who, "board").usage, "old {unit.path}");
            }
        }

        mod on_description_patch_only {
            use super::*;

            #[test]
            fn leaves_usage_unchanged() {
                let (mut f, who) = seeded();
                let updated = patch(
                    &mut f,
                    &who,
                    ResourcePatch {
                        description: Some("new".into()),
                        ..ResourcePatch::default()
                    },
                )
                .unwrap();
                assert_eq!(updated.description, "new");
                assert_eq!(updated.usage, "old {unit.path}");
            }

            #[test]
            fn refuses_a_multiline_description() {
                let (mut f, who) = seeded();
                let e = patch(
                    &mut f,
                    &who,
                    ResourcePatch {
                        description: Some("a\r\nb".into()),
                        ..ResourcePatch::default()
                    },
                )
                .unwrap_err();
                assert_eq!(e.code(), "description_multiline");
            }
        }

        mod on_any_text_change {
            use super::*;

            #[test]
            fn records_an_audit_entry() {
                let (mut f, who) = seeded();
                patch(
                    &mut f,
                    &who,
                    ResourcePatch {
                        usage: Some("x".into()),
                        ..ResourcePatch::default()
                    },
                )
                .unwrap();
                assert_eq!(f.audit_actions().last().unwrap(), "resource_updated");
            }
        }
    }

    mod add_unit {
        use super::*;

        fn base() -> (Fixture, Caller) {
            let mut f = fixture();
            let who = caller(None);
            f.store
                .tx(|tx| {
                    f.inv
                        .create_resource(tx, &who, &new_device("a", Scope::User))
                })
                .unwrap();
            (f, who)
        }

        fn unit_with(description: &str) -> NewUnit {
            NewUnit {
                description: description.into(),
                ..new_unit("u1", "/dev/ttyUSB0")
            }
        }

        mod on_valid_description {
            use super::*;

            #[test]
            fn stores_it() {
                let (mut f, who) = base();
                let created = f
                    .store
                    .tx(|tx| f.inv.add_unit(tx, &who, &rid("a"), &unit_with("left port")))
                    .unwrap();
                assert_eq!(created.value.description, "left port");
                assert_eq!(get(&mut f, &who, "a").units[0].description, "left port");
            }
        }

        mod on_too_long_or_multiline_description {
            use super::*;

            #[test]
            fn returns_invalid_text_for_too_long() {
                let (mut f, who) = base();
                let e = f
                    .store
                    .tx(|tx| {
                        f.inv
                            .add_unit(tx, &who, &rid("a"), &unit_with(&long_description()))
                    })
                    .unwrap_err();
                assert!(matches!(
                    e,
                    InventoryError::InvalidText(TextError::DescriptionTooLong { .. })
                ));
            }

            #[test]
            fn returns_invalid_text_for_multiline() {
                let (mut f, who) = base();
                let e = f
                    .store
                    .tx(|tx| f.inv.add_unit(tx, &who, &rid("a"), &unit_with("a\nb")))
                    .unwrap_err();
                assert!(matches!(
                    e,
                    InventoryError::InvalidText(TextError::DescriptionMultiline)
                ));
            }

            #[test]
            fn stores_nothing() {
                let (mut f, who) = base();
                let _ = f
                    .store
                    .tx(|tx| f.inv.add_unit(tx, &who, &rid("a"), &unit_with("a\nb")));
                assert_eq!(get(&mut f, &who, "a").units.len(), 0);
            }
        }
    }

    mod update_unit {
        use super::*;

        fn seeded() -> (Fixture, Caller) {
            let mut f = fixture();
            let who = caller(None);
            f.seed(&who, "a", Scope::User, "u1");
            (f, who)
        }

        fn patch(
            f: &mut Fixture,
            who: &Caller,
            description: &str,
        ) -> Result<leclerk::inventory::Created<leclerk::model::Unit>, InventoryError> {
            f.store.tx(|tx| {
                f.inv.update_unit(
                    tx,
                    who,
                    &rid("a"),
                    &uid("u1"),
                    UnitPatch {
                        settings: None,
                        description: Some(description.into()),
                    },
                )
            })
        }

        mod on_valid_description {
            use super::*;

            #[test]
            fn stores_it() {
                let (mut f, who) = seeded();
                let updated = patch(&mut f, &who, "moved").unwrap();
                assert_eq!(updated.value.description, "moved");
            }
        }

        mod on_too_long_or_multiline_description {
            use super::*;

            #[test]
            fn returns_invalid_text() {
                let (mut f, who) = seeded();
                let e = patch(&mut f, &who, "a\nb").unwrap_err();
                assert!(matches!(e, InventoryError::InvalidText(_)));
            }

            #[test]
            fn returns_invalid_text_for_too_long() {
                let (mut f, who) = seeded();
                let e = patch(&mut f, &who, &long_description()).unwrap_err();
                assert_eq!(e.code(), "description_too_long");
            }

            #[test]
            fn keeps_the_old_description() {
                let (mut f, who) = seeded();
                patch(&mut f, &who, "kept").unwrap();
                let _ = patch(&mut f, &who, "a\nb");
                assert_eq!(get(&mut f, &who, "a").units[0].description, "kept");
            }
        }
    }

    /// A generic unit with a recognisable description.
    fn described_unit(id: &str, key: &str, description: &str) -> NewUnit {
        NewUnit {
            description: description.into(),
            ..generic_unit(id, key)
        }
    }

    /// A generic resource with a recognisable description and usage.
    fn described_resource(id: &str, scope: Scope, description: &str) -> NewResource {
        NewResource {
            description: description.into(),
            usage: "secret usage".into(),
            ..new_generic(id, scope)
        }
    }

    impl Fixture {
        /// Create a described generic resource and unit as a caller of `scope_path`.
        fn seed_described(
            &mut self,
            scope_path: Option<&str>,
            id: &str,
            (resource_text, unit_text): (&str, &str),
            key: &str,
        ) -> Result<leclerk::inventory::Created<leclerk::model::Unit>, InventoryError> {
            let who = caller(scope_path);
            self.store.tx(|tx| {
                self.inv.create_resource(
                    tx,
                    &who,
                    &described_resource(id, scope_of(scope_path), resource_text),
                )?;
                self.inv
                    .add_unit(tx, &who, &rid(id), &described_unit("u1", key, unit_text))
            })
        }
    }

    mod update_unit_duplicates {
        use super::*;

        mod on_new_settings_matching_a_unit_in_the_same_scope {
            use super::*;

            fn refused() -> InventoryError {
                let mut f = fixture();
                f.seed_described(None, "first", ("first board", "first port"), "k1")
                    .unwrap();
                f.seed_described(None, "second", ("second board", "second port"), "k2")
                    .unwrap();
                let who = caller(None);
                f.store
                    .tx(|tx| {
                        f.inv.update_unit(
                            tx,
                            &who,
                            &rid("second"),
                            &uid("u1"),
                            UnitPatch {
                                settings: Some(generic_settings("k1")),
                                description: None,
                            },
                        )
                    })
                    .unwrap_err()
            }

            #[test]
            fn duplicate_in_scope_carries_the_existing_descriptions() {
                assert!(matches!(
                    refused(),
                    InventoryError::DuplicateInScope {
                        ref resource,
                        ref unit,
                        ref resource_description,
                        ref unit_description,
                    } if *resource == rid("first")
                        && *unit == uid("u1")
                        && resource_description == "first board"
                        && unit_description == "first port"
                ));
            }

            #[test]
            fn details_include_both_descriptions() {
                let details = refused().details().unwrap();
                assert_eq!(details["resource"], "first");
                assert_eq!(details["unit"], "u1");
                assert_eq!(details["resource_description"], "first board");
                assert_eq!(details["unit_description"], "first port");
            }
        }
    }

    mod add_unit_duplicates {
        use super::*;

        mod on_match_in_same_scope {
            use super::*;

            fn refused() -> InventoryError {
                let mut f = fixture();
                f.seed_described(None, "first", ("first board", "first port"), "k")
                    .unwrap();
                f.seed_described(None, "second", ("second board", "second port"), "k")
                    .unwrap_err()
            }

            #[test]
            fn duplicate_in_scope_carries_the_existing_descriptions() {
                assert!(matches!(
                    refused(),
                    InventoryError::DuplicateInScope {
                        ref resource_description,
                        ref unit_description,
                        ..
                    } if resource_description == "first board" && unit_description == "first port"
                ));
            }

            #[test]
            fn details_include_both_descriptions() {
                let details = refused().details().unwrap();
                assert_eq!(details["resource"], "first");
                assert_eq!(details["unit"], "u1");
                assert_eq!(details["resource_description"], "first board");
                assert_eq!(details["unit_description"], "first port");
            }
        }

        mod on_match_with_a_user_scope_unit {
            use super::*;

            #[test]
            fn warns_with_resource_and_unit_description() {
                let mut f = fixture();
                f.seed_described(None, "shared", ("shared board", "shared port"), "k")
                    .unwrap();
                let created = f
                    .seed_described(Some("/p/a"), "mine", ("my board", "my port"), "k")
                    .unwrap();
                assert_eq!(
                    created.warnings,
                    vec![Warning::DuplicateOfUserUnit {
                        resource: rid("shared"),
                        unit: uid("u1"),
                        resource_description: "shared board".into(),
                        unit_description: "shared port".into(),
                    }]
                );
            }
        }

        mod on_match_with_own_project_unit_from_user_scope {
            use super::*;

            #[test]
            fn warns_with_resource_and_unit_description() {
                let mut f = fixture();
                f.seed_described(Some("/p/a"), "mine", ("my board", "my port"), "k")
                    .unwrap();
                let who = caller(Some("/p/a"));
                let created = f
                    .store
                    .tx(|tx| {
                        f.inv.create_resource(
                            tx,
                            &who,
                            &described_resource("board", Scope::User, "user board"),
                        )?;
                        f.inv
                            .add_unit(tx, &who, &rid("board"), &described_unit("u1", "k", "x"))
                    })
                    .unwrap();
                assert_eq!(
                    created.warnings,
                    vec![Warning::DuplicateOfProjectUnit {
                        resource: rid("mine"),
                        unit: uid("u1"),
                        resource_description: "my board".into(),
                        unit_description: "my port".into(),
                    }]
                );
            }
        }

        mod on_match_in_other_project {
            use super::*;

            fn warnings() -> String {
                let mut f = fixture();
                f.seed_described(Some("/p/b"), "theirs", ("their board", "their port"), "k")
                    .unwrap();
                let created = f
                    .seed_described(Some("/p/a"), "mine", ("my board", "my port"), "k")
                    .unwrap();
                serde_json::to_string(&created.warnings).unwrap()
            }

            #[test]
            fn carries_no_description_or_usage_fields() {
                let json = warnings();
                for leak in [
                    "description",
                    "usage",
                    "their board",
                    "their port",
                    "secret usage",
                ] {
                    assert!(!json.contains(leak), "{leak} leaked in {json}");
                }
            }

            #[test]
            fn is_still_similar_in_other_project() {
                assert!(warnings().contains("similar_in_other_project"));
            }
        }
    }

    mod list_promotions {
        use super::*;

        mod on_a_pending_promotion {
            use super::*;

            #[test]
            fn carries_the_resources_description() {
                let mut f = fixture();
                f.seed_described(Some("/p/b"), "theirs", ("their board", "t"), "k")
                    .unwrap();
                f.seed_described(Some("/p/a"), "mine", ("my board", "m"), "k")
                    .unwrap();
                let list = f.pending();
                assert_eq!(list[0].description, "their board");
            }

            #[test]
            fn reads_the_current_description_at_list_time() {
                let mut f = fixture();
                f.seed_described(Some("/p/b"), "theirs", ("their board", "t"), "k")
                    .unwrap();
                f.seed_described(Some("/p/a"), "mine", ("my board", "m"), "k")
                    .unwrap();
                let b = caller(Some("/p/b"));
                f.store
                    .tx(|tx| {
                        f.inv.update_resource(
                            tx,
                            &b,
                            &rid("theirs"),
                            ResourcePatch {
                                description: Some("renamed".into()),
                                ..ResourcePatch::default()
                            },
                        )
                    })
                    .unwrap();
                assert_eq!(f.pending()[0].description, "renamed");
            }
        }
    }
}

mod store_migration_0010 {
    use leclerk::store::migrations;
    use rusqlite::Connection;

    mod on_a_database_at_version_9_with_notes_data {
        use super::*;

        fn migrated() -> Connection {
            let mut conn = Connection::open_in_memory().unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            migrations().to_version(&mut conn, 9).unwrap();
            conn.execute(
                "INSERT INTO resources (id, scope_kind, type, description, notes, created_at) \
                 VALUES ('board', 'user', 'generic', 'boards', 'resource notes', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO units (resource_row, id, settings, notes, created_at) \
                 VALUES (1, 'u1', '{}', 'unit notes', 1)",
                [],
            )
            .unwrap();
            migrations().to_latest(&mut conn).unwrap();
            conn
        }

        #[test]
        fn resources_usage_holds_the_former_resources_notes() {
            let conn = migrated();
            let usage: String = conn
                .query_row("SELECT usage FROM resources", [], |r| r.get(0))
                .unwrap();
            assert_eq!(usage, "resource notes");
        }

        #[test]
        fn units_description_holds_the_former_units_notes() {
            let conn = migrated();
            let description: String = conn
                .query_row("SELECT description FROM units", [], |r| r.get(0))
                .unwrap();
            assert_eq!(description, "unit notes");
        }

        #[test]
        fn resources_description_is_untouched() {
            let conn = migrated();
            let description: String = conn
                .query_row("SELECT description FROM resources", [], |r| r.get(0))
                .unwrap();
            assert_eq!(description, "boards");
        }

        #[test]
        fn the_notes_columns_are_gone() {
            let conn = migrated();
            assert!(conn.prepare("SELECT notes FROM resources").is_err());
            assert!(conn.prepare("SELECT notes FROM units").is_err());
        }
    }
}

mod resource_tags {
    use leclerk::error::ErrorCode;
    use leclerk::model::{Tag, TagError};
    use leclerk::store::StoreError;
    use serde_json::{Value, json};

    use super::*;

    const ESP32: [&str; 2] = ["esp32", "zigbee"];

    fn strings(tags: &[&str]) -> Vec<String> {
        tags.iter().map(|t| (*t).to_owned()).collect()
    }

    fn tagged(id: &str, scope: Scope, tags: &[&str]) -> NewResource {
        NewResource {
            tags: strings(tags),
            ..new_generic(id, scope)
        }
    }

    fn names(resource: &Resource) -> Vec<&str> {
        resource.tags.iter().map(Tag::as_str).collect()
    }

    fn create(
        f: &mut Fixture,
        who: &Caller,
        new: &NewResource,
    ) -> Result<Resource, InventoryError> {
        f.store
            .tx(|tx| f.inv.create_resource(tx, who, new))
            .map(|c| c.value)
    }

    fn get(f: &mut Fixture, who: &Caller, id: &str) -> Resource {
        f.store
            .tx(|tx| f.inv.get_resource(tx, who, &rid(id)))
            .unwrap()
    }

    fn update(
        f: &mut Fixture,
        who: &Caller,
        id: &str,
        patch: ResourcePatch,
    ) -> Result<Resource, InventoryError> {
        f.store
            .tx(|tx| f.inv.update_resource(tx, who, &rid(id), patch))
    }

    fn last_detail(f: &mut Fixture, action: &str) -> Value {
        f.store
            .tx(audit::all)
            .unwrap()
            .into_iter()
            .rev()
            .find(|e| e.action == action)
            .unwrap()
            .detail
    }

    fn list(f: &mut Fixture, who: &Caller, view: View, tags: &[&str]) -> Vec<String> {
        let tags = strings(tags);
        f.store
            .tx(|tx| f.inv.list_resources(tx, who, view, &tags))
            .unwrap()
            .into_iter()
            .map(|r| r.id.to_string())
            .collect()
    }

    /// A user resource `a` tagged esp32, zigbee.
    fn seeded() -> (Fixture, Caller) {
        let mut f = fixture();
        let who = caller(None);
        create(&mut f, &who, &tagged("a", Scope::User, &ESP32)).unwrap();
        (f, who)
    }

    mod create_resource {
        use super::*;

        mod on_valid_tags {
            use super::*;

            #[test]
            fn stores_them_and_get_resource_returns_them_sorted_and_deduplicated() {
                let mut f = fixture();
                let who = caller(None);
                let new = tagged("a", Scope::User, &["zigbee", "esp32", "zigbee"]);
                create(&mut f, &who, &new).unwrap();
                assert_eq!(names(&get(&mut f, &who, "a")), ["esp32", "zigbee"]);
            }
        }

        mod on_a_malformed_tag {
            use super::*;

            #[test]
            fn returns_invalid_tag_naming_it_and_stores_nothing() {
                let mut f = fixture();
                let who = caller(None);
                let err =
                    create(&mut f, &who, &tagged("a", Scope::User, &["ok", "Thread"])).unwrap_err();
                assert!(matches!(
                    &err,
                    InventoryError::InvalidTag(TagError::Invalid { tag }) if tag == "Thread"
                ));
                assert_eq!(err.code(), "invalid_tag");
                assert_eq!(err.details(), Some(json!({"tag": "Thread"})));
                assert!(
                    f.store
                        .tx(|tx| f.inv.get_resource(tx, &who, &rid("a")))
                        .is_err()
                );
                assert_eq!(f.audit_actions(), Vec::<String>::new());
            }

            #[test]
            fn refuses_an_agent_guess_at_the_format() {
                let mut f = fixture();
                let who = caller(None);
                let long = "x".repeat(33);
                for bad in ["esp32 c6", "zigbee,thread", "-x", long.as_str(), ""] {
                    let err = create(&mut f, &who, &tagged("a", Scope::User, &[bad])).unwrap_err();
                    assert_eq!(err.code(), "invalid_tag", "{bad:?}");
                    assert_eq!(err.details(), Some(json!({"tag": bad})));
                }
            }
        }

        mod on_17_tags {
            use super::*;

            #[test]
            fn returns_too_many_tags_and_stores_nothing() {
                let mut f = fixture();
                let who = caller(None);
                let many: Vec<String> = (0..17).map(|i| format!("t{i:02}")).collect();
                let refs: Vec<&str> = many.iter().map(String::as_str).collect();
                let err = create(&mut f, &who, &tagged("a", Scope::User, &refs)).unwrap_err();
                assert!(matches!(
                    err,
                    InventoryError::InvalidTag(TagError::TooMany { max: 16, count: 17 })
                ));
                assert_eq!(err.code(), "too_many_tags");
                assert!(
                    f.store
                        .tx(|tx| f.inv.get_resource(tx, &who, &rid("a")))
                        .is_err()
                );
            }
        }

        mod on_success {
            use super::*;

            #[test]
            fn the_resource_created_audit_entry_carries_the_tags() {
                let (mut f, _) = seeded();
                let detail = last_detail(&mut f, "resource_created");
                assert_eq!(detail["tags"], json!(["esp32", "zigbee"]));
            }
        }
    }

    mod update_resource {
        use super::*;

        fn patch() -> ResourcePatch {
            ResourcePatch::default()
        }

        mod on_tags_replace {
            use super::*;

            #[test]
            fn the_stored_set_equals_the_new_one() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    tags: Some(strings(&["thread", "c6", "thread"])),
                    ..patch()
                };
                let updated = update(&mut f, &who, "a", p).unwrap();
                assert_eq!(names(&updated), ["c6", "thread"]);
                assert_eq!(names(&get(&mut f, &who, "a")), ["c6", "thread"]);
            }
        }

        mod on_tags_empty {
            use super::*;

            #[test]
            fn clears_all_tags() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    tags: Some(Vec::new()),
                    ..patch()
                };
                update(&mut f, &who, "a", p).unwrap();
                assert!(get(&mut f, &who, "a").tags.is_empty());
            }
        }

        mod on_add_tags {
            use super::*;

            #[test]
            fn keeps_the_existing_tags_and_adds_the_new_ones() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    add_tags: strings(&["thread"]),
                    ..patch()
                };
                let updated = update(&mut f, &who, "a", p).unwrap();
                assert_eq!(names(&updated), ["esp32", "thread", "zigbee"]);
                assert_eq!(
                    names(&get(&mut f, &who, "a")),
                    ["esp32", "thread", "zigbee"]
                );
            }
        }

        mod on_remove_tags_including_one_not_present {
            use super::*;

            #[test]
            fn removes_the_present_ones_and_the_absent_one_is_a_no_op() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    remove_tags: strings(&["zigbee", "thread"]),
                    ..patch()
                };
                let updated = update(&mut f, &who, "a", p).unwrap();
                assert_eq!(names(&updated), ["esp32"]);
            }
        }

        mod on_add_beyond_the_limit {
            use super::*;

            #[test]
            fn returns_too_many_tags_and_the_stored_set_is_unchanged() {
                let mut f = fixture();
                let who = caller(None);
                let sixteen: Vec<String> = (0..16).map(|i| format!("t{i:02}")).collect();
                let refs: Vec<&str> = sixteen.iter().map(String::as_str).collect();
                create(&mut f, &who, &tagged("a", Scope::User, &refs)).unwrap();
                let p = ResourcePatch {
                    add_tags: strings(&["one-more"]),
                    description: Some("changed".into()),
                    ..patch()
                };
                let err = update(&mut f, &who, "a", p).unwrap_err();
                assert_eq!(err.code(), "too_many_tags");
                assert_eq!(err.details(), Some(json!({"max": 16, "count": 17})));
                let after = get(&mut f, &who, "a");
                assert_eq!(after.tags.len(), 16);
                assert_eq!(after.description, "a board");
            }
        }

        mod on_tags_together_with_add_or_remove {
            use super::*;

            fn assert_conflict(f: &mut Fixture, who: &Caller, p: ResourcePatch) {
                let err = update(f, who, "a", p).unwrap_err();
                assert!(matches!(
                    err,
                    InventoryError::ConflictingTagEdit {
                        reason: "replace_with_add_or_remove",
                        tag: None
                    }
                ));
                assert_eq!(err.code(), "conflicting_tag_edit");
                assert_eq!(
                    err.details(),
                    Some(json!({"reason": "replace_with_add_or_remove"}))
                );
                assert_eq!(names(&get(f, who, "a")), ["esp32", "zigbee"]);
            }

            #[test]
            fn tags_with_add_tags_returns_conflicting_tag_edit_and_changes_nothing() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    tags: Some(strings(&["x"])),
                    add_tags: strings(&["y"]),
                    ..patch()
                };
                assert_conflict(&mut f, &who, p);
            }

            #[test]
            fn tags_with_remove_tags_returns_conflicting_tag_edit_and_changes_nothing() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    tags: Some(Vec::new()),
                    remove_tags: strings(&["esp32"]),
                    ..patch()
                };
                assert_conflict(&mut f, &who, p);
            }
        }

        mod on_a_tag_in_both_add_and_remove {
            use super::*;

            #[test]
            fn returns_conflicting_tag_edit_naming_the_tag_and_changes_nothing() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    add_tags: strings(&["thread", "new"]),
                    remove_tags: strings(&["thread"]),
                    ..patch()
                };
                let err = update(&mut f, &who, "a", p).unwrap_err();
                assert!(matches!(
                    &err,
                    InventoryError::ConflictingTagEdit {
                        reason: "added_and_removed",
                        tag: Some(tag)
                    } if tag == "thread"
                ));
                assert_eq!(
                    err.details(),
                    Some(json!({"reason": "added_and_removed", "tag": "thread"}))
                );
                assert_eq!(names(&get(&mut f, &who, "a")), ["esp32", "zigbee"]);
            }
        }

        mod on_add_tags_that_push_the_result_over_the_limit {
            use super::*;

            #[test]
            fn returns_too_many_tags_counting_the_resulting_set_and_changes_nothing() {
                let (mut f, who) = seeded();
                create(
                    &mut f,
                    &who,
                    &tagged("b", Scope::User, &["a1", "a2", "a3", "a4", "a5"]),
                )
                .unwrap();
                let news: Vec<String> = (0..17).map(|i| format!("n{i:02}")).collect();
                let p = ResourcePatch {
                    add_tags: news,
                    ..patch()
                };
                let err = update(&mut f, &who, "b", p).unwrap_err();
                assert_eq!(err.code(), "too_many_tags");
                assert_eq!(err.details(), Some(json!({"max": 16, "count": 22})));
                assert_eq!(
                    names(&get(&mut f, &who, "b")),
                    ["a1", "a2", "a3", "a4", "a5"]
                );
            }
        }

        mod on_the_same_malformed_tag_added_and_removed {
            use super::*;

            #[test]
            fn returns_invalid_tag_not_a_conflict() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    add_tags: strings(&["Thread"]),
                    remove_tags: strings(&["Thread"]),
                    ..patch()
                };
                let err = update(&mut f, &who, "a", p).unwrap_err();
                assert_eq!(err.code(), "invalid_tag");
                assert_eq!(err.details(), Some(json!({"tag": "Thread"})));
                assert_eq!(names(&get(&mut f, &who, "a")), ["esp32", "zigbee"]);
            }
        }

        mod on_a_malformed_tag {
            use super::*;

            #[test]
            fn returns_invalid_tag_and_changes_nothing() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    add_tags: strings(&["fine", "Thread"]),
                    description: Some("changed".into()),
                    ..patch()
                };
                let err = update(&mut f, &who, "a", p).unwrap_err();
                assert_eq!(err.code(), "invalid_tag");
                assert_eq!(err.details(), Some(json!({"tag": "Thread"})));
                let after = get(&mut f, &who, "a");
                assert_eq!(names(&after), ["esp32", "zigbee"]);
                assert_eq!(after.description, "a board");
            }

            #[test]
            fn a_malformed_removal_is_refused_too() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    remove_tags: strings(&["esp32 c6"]),
                    ..patch()
                };
                let err = update(&mut f, &who, "a", p).unwrap_err();
                assert_eq!(err.code(), "invalid_tag");
            }
        }

        mod on_no_tag_fields {
            use super::*;

            #[test]
            fn leaves_the_tags_unchanged() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    description: Some("changed".into()),
                    ..patch()
                };
                let updated = update(&mut f, &who, "a", p).unwrap();
                assert_eq!(names(&updated), ["esp32", "zigbee"]);
            }
        }

        mod on_success {
            use super::*;

            #[test]
            fn the_resource_updated_audit_entry_carries_the_resulting_tags() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    add_tags: strings(&["thread"]),
                    ..patch()
                };
                update(&mut f, &who, "a", p).unwrap();
                let detail = last_detail(&mut f, "resource_updated");
                assert_eq!(detail["tags"], json!(["esp32", "thread", "zigbee"]));
            }

            #[test]
            fn the_audit_tags_are_the_unchanged_set_when_no_tag_field_is_given() {
                let (mut f, who) = seeded();
                let p = ResourcePatch {
                    description: Some("changed".into()),
                    ..patch()
                };
                update(&mut f, &who, "a", p).unwrap();
                let detail = last_detail(&mut f, "resource_updated");
                assert_eq!(detail["tags"], json!(["esp32", "zigbee"]));
            }
        }
    }

    mod list_resources {
        use super::*;

        /// User resources a (esp32, zigbee), b (esp32, thread), c (none).
        fn listed() -> (Fixture, Caller) {
            let (mut f, who) = seeded();
            create(
                &mut f,
                &who,
                &tagged("b", Scope::User, &["esp32", "thread"]),
            )
            .unwrap();
            create(&mut f, &who, &tagged("c", Scope::User, &[])).unwrap();
            (f, who)
        }

        mod on_no_tags {
            use super::*;

            #[test]
            fn returns_every_resource_in_the_view() {
                let (mut f, who) = listed();
                assert_eq!(list(&mut f, &who, View::Caller, &[]), ["a", "b", "c"]);
            }
        }

        mod on_one_tag {
            use super::*;

            #[test]
            fn returns_only_resources_carrying_it() {
                let (mut f, who) = listed();
                assert_eq!(list(&mut f, &who, View::Caller, &["esp32"]), ["a", "b"]);
            }
        }

        mod on_several_tags {
            use super::*;

            #[test]
            fn returns_only_resources_carrying_all_of_them() {
                let (mut f, who) = listed();
                assert_eq!(
                    list(&mut f, &who, View::Caller, &["esp32", "thread"]),
                    ["b"]
                );
            }
        }

        mod on_a_tag_nobody_has {
            use super::*;

            #[test]
            fn returns_an_empty_list() {
                let (mut f, who) = listed();
                assert_eq!(
                    list(&mut f, &who, View::Caller, &["matter"]),
                    Vec::<String>::new()
                );
            }
        }

        mod on_match_in_another_project {
            use super::*;

            #[test]
            fn the_other_projects_resource_is_not_returned() {
                let mut f = fixture();
                let (a, b) = (caller(Some("/p/a")), caller(Some("/p/b")));
                create(
                    &mut f,
                    &b,
                    &tagged("theirs", Scope::Project(project("/p/b")), &["thread"]),
                )
                .unwrap();
                create(
                    &mut f,
                    &a,
                    &tagged("mine", Scope::Project(project("/p/a")), &["thread"]),
                )
                .unwrap();
                assert_eq!(list(&mut f, &a, View::Caller, &["thread"]), ["mine"]);
                assert_eq!(list(&mut f, &b, View::Caller, &["thread"]), ["theirs"]);
            }

            #[test]
            fn a_caller_without_a_project_sees_no_project_match() {
                let mut f = fixture();
                let b = caller(Some("/p/b"));
                create(
                    &mut f,
                    &b,
                    &tagged("theirs", Scope::Project(project("/p/b")), &["thread"]),
                )
                .unwrap();
                assert_eq!(
                    list(&mut f, &caller(None), View::Caller, &["thread"]),
                    Vec::<String>::new()
                );
            }
        }

        mod on_view_all {
            use super::*;

            #[test]
            fn filters_across_every_scope() {
                let mut f = fixture();
                let (a, b) = (caller(Some("/p/a")), caller(Some("/p/b")));
                create(&mut f, &a, &tagged("user", Scope::User, &["thread"])).unwrap();
                create(
                    &mut f,
                    &b,
                    &tagged("theirs", Scope::Project(project("/p/b")), &["thread"]),
                )
                .unwrap();
                create(
                    &mut f,
                    &b,
                    &tagged("other", Scope::Project(project("/p/b")), &["zigbee"]),
                )
                .unwrap();
                assert_eq!(
                    list(&mut f, &caller(None), View::All, &["thread"]),
                    ["theirs", "user"]
                );
            }
        }

        mod on_a_malformed_filter_tag {
            use super::*;

            #[test]
            fn returns_invalid_tag_and_no_listing() {
                let (mut f, who) = listed();
                let tags = strings(&["esp32", "Thread"]);
                let err = f
                    .store
                    .tx(|tx| f.inv.list_resources(tx, &who, View::Caller, &tags))
                    .unwrap_err();
                assert_eq!(err.code(), "invalid_tag");
                assert_eq!(err.details(), Some(json!({"tag": "Thread"})));
            }
        }
    }

    mod duplicate_warnings {
        use leclerk::inventory::Created;
        use leclerk::model::Unit;

        use super::*;

        type Added = Result<Created<Unit>, InventoryError>;

        fn tagged_with_unit(
            f: &mut Fixture,
            who: &Caller,
            id: &str,
            scope: Scope,
            key: &str,
        ) -> Added {
            create(f, who, &tagged(id, scope, &["secret-tag"]))?;
            f.store.tx(|tx| {
                f.inv.add_unit(
                    tx,
                    who,
                    &rid(id),
                    &NewUnit {
                        id: uid("u1"),
                        settings: generic_settings(key),
                        description: String::new(),
                    },
                )
            })
        }

        fn assert_untagged(text: &str) {
            assert!(!text.contains("secret-tag"), "{text}");
            assert!(!text.contains("tags"), "{text}");
        }

        fn warnings_of(added: Added) -> Vec<Warning> {
            let warnings = added.unwrap().warnings;
            assert_untagged(&serde_json::to_string(&warnings).unwrap());
            assert_untagged(&format!("{warnings:?}"));
            warnings
        }

        mod add_unit {
            use super::*;

            #[test]
            fn duplicate_in_scope_carries_no_tags() {
                let mut f = fixture();
                let who = caller(None);
                tagged_with_unit(&mut f, &who, "a", Scope::User, "k").unwrap();
                let err = tagged_with_unit(&mut f, &who, "b", Scope::User, "k").unwrap_err();
                assert!(matches!(err, InventoryError::DuplicateInScope { .. }));
                assert_untagged(&format!("{err:?} {:?}", err.details()));
            }

            #[test]
            fn duplicate_of_user_unit_carries_no_tags() {
                let mut f = fixture();
                tagged_with_unit(&mut f, &caller(None), "u", Scope::User, "k").unwrap();
                let a = caller(Some("/p/a"));
                let added = tagged_with_unit(&mut f, &a, "m", Scope::Project(project("/p/a")), "k");
                let warnings = warnings_of(added);
                assert!(matches!(
                    warnings.as_slice(),
                    [Warning::DuplicateOfUserUnit { .. }]
                ));
            }

            #[test]
            fn duplicate_of_project_unit_carries_no_tags() {
                let mut f = fixture();
                let a = caller(Some("/p/a"));
                tagged_with_unit(&mut f, &a, "m", Scope::Project(project("/p/a")), "k").unwrap();
                let warnings = warnings_of(tagged_with_unit(&mut f, &a, "u", Scope::User, "k"));
                assert!(matches!(
                    warnings.as_slice(),
                    [Warning::DuplicateOfProjectUnit { .. }]
                ));
            }

            #[test]
            fn similar_in_other_project_carries_no_tags() {
                let mut f = fixture();
                let (a, b) = (caller(Some("/p/a")), caller(Some("/p/b")));
                tagged_with_unit(&mut f, &a, "m", Scope::Project(project("/p/a")), "k").unwrap();
                let added = tagged_with_unit(&mut f, &b, "t", Scope::Project(project("/p/b")), "k");
                let warnings = warnings_of(added);
                assert!(matches!(
                    warnings.as_slice(),
                    [Warning::SimilarInOtherProject { .. }]
                ));
            }
        }

        mod update_unit {
            use super::*;

            #[test]
            fn duplicate_in_scope_carries_no_tags() {
                let mut f = fixture();
                let who = caller(None);
                tagged_with_unit(&mut f, &who, "a", Scope::User, "k1").unwrap();
                tagged_with_unit(&mut f, &who, "b", Scope::User, "k2").unwrap();
                let err = f
                    .store
                    .tx(|tx| {
                        f.inv.update_unit(
                            tx,
                            &who,
                            &rid("b"),
                            &uid("u1"),
                            UnitPatch {
                                settings: Some(generic_settings("k1")),
                                ..UnitPatch::default()
                            },
                        )
                    })
                    .unwrap_err();
                assert!(matches!(err, InventoryError::DuplicateInScope { .. }));
                assert_untagged(&format!("{err:?} {:?}", err.details()));
            }
        }
    }

    mod store_resource_delete {
        use super::*;

        mod on_resource_delete {
            use super::*;

            #[test]
            fn its_resource_tags_rows_are_gone() {
                let (mut f, who) = seeded();
                create(&mut f, &who, &tagged("keep", Scope::User, &["x"])).unwrap();
                f.store
                    .tx(|tx| f.inv.delete_resource(tx, &who, &rid("a")))
                    .unwrap();
                let rows: Vec<String> = f
                    .store
                    .tx(|tx| -> Result<_, StoreError> {
                        let mut stmt = tx.conn().prepare("SELECT tag FROM resource_tags")?;
                        Ok(stmt
                            .query_map([], |r| r.get(0))?
                            .collect::<Result<_, _>>()?)
                    })
                    .unwrap();
                assert_eq!(rows, ["x"]);
            }
        }
    }

    mod migration_0011 {
        use leclerk::store::migrations;
        use rusqlite::Connection;

        mod on_a_database_at_version_10_with_resources {
            use super::*;

            #[test]
            fn those_resources_have_no_tags_after_migrating() {
                let mut conn = Connection::open_in_memory().unwrap();
                conn.pragma_update(None, "foreign_keys", "ON").unwrap();
                migrations().to_version(&mut conn, 10).unwrap();
                conn.execute(
                    "INSERT INTO resources (id, scope_kind, type, description, usage, created_at) \
                     VALUES ('board', 'user', 'generic', 'boards', '', 1)",
                    [],
                )
                .unwrap();
                migrations().to_latest(&mut conn).unwrap();
                let tags: i64 = conn
                    .query_row("SELECT COUNT(*) FROM resource_tags", [], |r| r.get(0))
                    .unwrap();
                let resources: i64 = conn
                    .query_row("SELECT COUNT(*) FROM resources", [], |r| r.get(0))
                    .unwrap();
                assert_eq!((resources, tags), (1, 0));
            }
        }
    }
}
