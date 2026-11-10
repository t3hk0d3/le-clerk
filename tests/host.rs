//! Agent-session hosts and liveness (`scheduler::session`): host reports,
//! unhosting, the grace and its expiry, probing hosts, boot, and ending a
//! session from its host (`SessionEnd`). Driven on the scheduler with a
//! manual clock and real processes as hosts, plus the engine wiring.

use std::sync::Arc;
use std::time::{Duration, Instant};

use leclerk::clock::ManualClock;
use leclerk::config::Config;
use leclerk::engine::{ApiError, Command as EngineCommand, Engine, EngineHandle, EngineOptions};
use leclerk::health::HealthResult;
use leclerk::inventory::{Inventory, NewResource, NewUnit};
use leclerk::model::{
    Caller, HealthSpec, HolderRef, Host, LeaseId, Priority, RequestId, ResourceId, ResourceType,
    Scope, UnitId, UnitSettings,
};
use leclerk::scheduler::liveness;
use leclerk::scheduler::session::{Hosting, Reported, Sessions};
use leclerk::scheduler::{AcquireOutcome, AcquireRequest, HealthProbe, Scheduler, Target};
use leclerk::store::audit::{self, AuditEntry};
use leclerk::store::notices::{self, NoticeKind};
use leclerk::store::{Store, StoreError, Tx};
use std::process::{Child, Command};

const T0: i64 = 1_000_000;
const GRACE: Duration = Duration::from_secs(60);
const GRACE_MS: i64 = 60_000;

fn agent(id: &str) -> HolderRef {
    HolderRef::AgentSession {
        session_id: id.into(),
    }
}

fn user(tag: &str) -> HolderRef {
    HolderRef::User { tag: tag.into() }
}

fn as_caller(holder: HolderRef) -> Caller {
    Caller {
        holder,
        project: None,
        override_reason: None,
    }
}

fn req() -> AcquireRequest {
    AcquireRequest {
        target: Target::Resource(ResourceId::parse("board").unwrap()),
        duration: Duration::from_secs(3600),
        interruptible: false,
        priority: Priority::Normal,
        purpose: "test".into(),
        override_reason: None,
        skip_checks: false,
    }
}

struct Healthy;

impl HealthProbe for Healthy {
    fn check(
        &self,
        _spec: &HealthSpec,
        _resource: &ResourceId,
        _unit: &UnitId,
        _settings: &UnitSettings,
    ) -> HealthResult {
        HealthResult::Healthy
    }
}

/// Seed `board` with one unit `u1`.
fn seed(store: &mut Store, inv: &Inventory) {
    let who = as_caller(user("admin"));
    store
        .tx(|tx| -> Result<(), leclerk::inventory::InventoryError> {
            inv.create_resource(
                tx,
                &who,
                &NewResource {
                    id: ResourceId::parse("board").unwrap(),
                    ty: ResourceType::Generic,
                    scope: Scope::User,
                    description: String::new(),
                    usage: String::new(),
                    health: None,
                    tags: Vec::new(),
                },
            )?;
            inv.add_unit(
                tx,
                &who,
                &ResourceId::parse("board").unwrap(),
                &NewUnit {
                    id: UnitId::parse("u1").unwrap(),
                    settings: UnitSettings::Generic {
                        identity_key: "board-u1".into(),
                    },
                    description: String::new(),
                },
            )?;
            Ok(())
        })
        .unwrap();
}

/// The lease state, end cause and holder of `lease`.
fn lease_row(tx: &Tx, lease: &LeaseId) -> (String, Option<String>, HolderRef) {
    tx.conn()
        .query_row(
            "SELECT state, end_cause, holder FROM leases WHERE lease_id = ?1",
            [lease.as_str()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .map(|(s, c, h)| (s, c, serde_json::from_str(&h).unwrap()))
        .unwrap()
}

/// The holders of the queued requests.
fn queued_holders(tx: &Tx) -> Vec<HolderRef> {
    let mut stmt = tx
        .conn()
        .prepare("SELECT holder FROM requests WHERE state = 'queued' ORDER BY row_id")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|h| serde_json::from_str(&h.unwrap()).unwrap())
        .collect()
}

// ---------------------------------------------------------------------------
// Hosts: real processes
// ---------------------------------------------------------------------------

/// A process standing for a Claude Code process; killed when dropped.
struct Proc {
    child: Child,
    host: Host,
}

impl Proc {
    fn spawn() -> Self {
        let child = Command::new("sleep").arg("300").spawn().unwrap();
        let pid = child.id();
        let start_time = liveness::start_time(pid).expect("a running child has a start time");
        Self {
            child,
            host: Host {
                pid,
                start_time,
                boot_id: None,
            },
        }
    }

    /// Kill it and reap it: its host is gone.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

// ---------------------------------------------------------------------------
// Scheduler-level fixture
// ---------------------------------------------------------------------------

struct Fixture {
    store: Store,
    sched: Scheduler,
    clock: Arc<ManualClock>,
    sessions: Sessions,
}

fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(T0));
    let inv = Inventory::new(clock.clone());
    let mut store = Store::open_in_memory().unwrap();
    seed(&mut store, &inv);
    Fixture {
        store,
        sched: Scheduler::new(clock.clone(), Config::default(), Arc::new(Healthy)),
        clock,
        sessions: Sessions::new(GRACE),
    }
}

impl Fixture {
    fn now(&self) -> i64 {
        leclerk::clock::Clock::now_ms(&*self.clock)
    }

    fn at(&self, ms: i64) {
        self.clock.set(ms);
    }

    fn with_tx<T>(&mut self, f: impl FnOnce(&Tx) -> T) -> T {
        self.store
            .tx(|tx| -> Result<T, StoreError> { Ok(f(tx)) })
            .unwrap()
    }

    fn acquire(&mut self, holder: HolderRef) -> AcquireOutcome {
        let who = as_caller(holder);
        self.store
            .tx(|tx| self.sched.acquire(tx, &who, req()))
            .unwrap()
    }

    fn grant(&mut self, holder: HolderRef) -> LeaseId {
        match self.acquire(holder) {
            AcquireOutcome::Granted(g) => g.lease_id,
            other => panic!("expected grant, got {other:?}"),
        }
    }

    fn queue(&mut self, holder: HolderRef) -> RequestId {
        match self.acquire(holder) {
            AcquireOutcome::Queued { request_id, .. } => request_id,
            other => panic!("expected queued, got {other:?}"),
        }
    }

    fn report(&mut self, session: &str, host: Host) -> Reported {
        let now = self.now();
        let sessions = &mut self.sessions;
        self.store
            .tx(|tx| sessions.report(tx, session, host, now))
            .unwrap()
    }

    fn touch(&mut self, session: &str, host: Option<Host>) -> Hosting {
        let now = self.now();
        let sessions = &mut self.sessions;
        self.store
            .tx(|tx| sessions.touch(tx, session, host, now))
            .unwrap()
    }

    fn sweep(&mut self) -> Vec<String> {
        let now = self.now();
        let sessions = &mut self.sessions;
        self.store
            .tx(|tx| sessions.sweep_hostless(tx, now))
            .unwrap()
    }

    fn probe(&mut self) -> Vec<String> {
        let now = self.now();
        let sessions = &mut self.sessions;
        self.store.tx(|tx| sessions.probe(tx, now)).unwrap()
    }

    fn expire(&mut self) -> Vec<String> {
        let now = self.now();
        let (sessions, sched) = (&mut self.sessions, &self.sched);
        self.store.tx(|tx| sessions.expire(tx, sched, now)).unwrap()
    }

    /// A daemon restart: the record starts over from the store.
    fn restart(&mut self) -> Vec<String> {
        self.sessions = Sessions::new(GRACE);
        let now = self.now();
        let sessions = &mut self.sessions;
        self.store.tx(|tx| sessions.boot(tx, now)).unwrap()
    }

    fn end(&mut self, session: &str, host: Option<Host>, reason: &str) -> bool {
        let (sessions, sched) = (&mut self.sessions, &self.sched);
        self.store
            .tx(|tx| sessions.end(tx, sched, session, host, reason))
            .unwrap()
            .is_some()
    }

    fn audit(&mut self, action: &str) -> Vec<AuditEntry> {
        self.store
            .tx(audit::all)
            .unwrap()
            .into_iter()
            .filter(|e| e.action == action)
            .collect()
    }

    fn lease(&mut self, lease: &LeaseId) -> (String, Option<String>, HolderRef) {
        self.with_tx(|tx| lease_row(tx, lease))
    }

    fn queued(&mut self) -> Vec<HolderRef> {
        self.with_tx(queued_holders)
    }

    fn offered_to(&mut self, holder: &HolderRef) -> bool {
        self.with_tx(|tx| notices::pending(tx, holder).unwrap())
            .iter()
            .any(|n| n.kind == NoticeKind::Offered)
    }

    /// `s1` hosted by `p` holds `board/u1`, and user `w` waits for it.
    fn held_by_s1(&mut self, p: &Proc) -> LeaseId {
        self.report("s1", p.host.clone());
        let lease = self.grant(agent("s1"));
        self.queue(user("w"));
        lease
    }
}

// ---------------------------------------------------------------------------
// scheduler::session
// ---------------------------------------------------------------------------

mod scheduler {
    use super::*;

    mod session {
        use super::*;

        mod report {
            use super::*;

            mod on_a_sessions_first_report {
                use super::*;

                #[test]
                fn records_its_host() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    let reported = f.report("s1", p.host.clone());
                    assert_eq!(reported.unhosted, Vec::<String>::new());
                    assert_eq!(f.sessions.host_of("s1"), Some(p.host.clone()));
                    assert!(f.sessions.is_alive("s1"));
                }
            }

            mod on_session_n_reported_by_a_process_that_hosted_u {
                use super::*;

                #[test]
                fn u_becomes_unhosted_and_its_grace_starts() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    f.held_by_s1(&p);
                    let reported = f.report("s2", p.host.clone());
                    assert_eq!(reported.unhosted, vec!["s1".to_owned()]);
                    assert_eq!(f.sessions.host_of("s1"), None);
                    assert!(!f.sessions.is_alive("s1"));
                    assert_eq!(f.sessions.grace_ends("s1"), Some(T0 + GRACE_MS));
                    assert!(f.sessions.is_alive("s2"));
                }

                #[test]
                fn its_leases_stay_held_during_the_grace() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    let lease = f.held_by_s1(&p);
                    f.report("s2", p.host.clone());
                    f.at(T0 + GRACE_MS - 1);
                    assert_eq!(f.expire(), Vec::<String>::new());
                    assert_eq!(f.lease(&lease).0, "held");
                }
            }

            mod on_u_reported_again_within_its_grace {
                use super::*;

                #[test]
                fn is_rehosted_by_that_process_the_grace_is_cancelled_and_nothing_ends() {
                    let mut f = fixture();
                    let mut old = Proc::spawn();
                    let lease = f.held_by_s1(&old);
                    old.kill();
                    f.probe();
                    assert_eq!(f.sessions.grace_ends("s1"), Some(T0 + GRACE_MS));
                    f.at(T0 + GRACE_MS / 2);
                    let new = Proc::spawn();
                    f.report("s1", new.host.clone());
                    assert_eq!(f.sessions.host_of("s1"), Some(new.host.clone()));
                    assert_eq!(f.sessions.grace_ends("s1"), None);
                    f.at(T0 + 3 * GRACE_MS);
                    assert_eq!(f.expire(), Vec::<String>::new());
                    assert_eq!(f.lease(&lease).0, "held");
                }
            }

            mod on_a_report_from_a_reused_pid_with_another_start_time {
                use super::*;

                #[test]
                fn counts_as_a_different_process_and_unhosts_nothing_of_the_old_one() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    f.held_by_s1(&p);
                    let reused = Host {
                        pid: p.host.pid,
                        start_time: p.host.start_time + 1,
                        boot_id: None,
                    };
                    let reported = f.report("s2", reused);
                    assert_eq!(reported.unhosted, Vec::<String>::new());
                    assert_eq!(f.sessions.host_of("s1"), Some(p.host.clone()));
                    assert!(f.sessions.is_alive("s1"));
                }
            }
        }

        mod touch {
            use super::*;

            mod on_a_new_session {
                use super::*;

                #[test]
                fn records_its_host() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    assert_eq!(f.touch("s1", Some(p.host.clone())), Hosting::Hosted);
                    assert!(f.sessions.is_alive("s1"));
                }
            }

            mod on_a_host_listed_under_this_session {
                use super::*;

                #[test]
                fn returns_hosted() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    f.report("s1", p.host.clone());
                    assert_eq!(f.touch("s1", Some(p.host.clone())), Hosting::Hosted);
                }
            }

            mod on_a_host_listed_under_another_session {
                use super::*;

                #[test]
                fn returns_host_runs_other_naming_that_session() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    f.report("s1", p.host.clone());
                    assert_eq!(
                        f.touch("s2", Some(p.host.clone())),
                        Hosting::HostRunsOther("s1".into())
                    );
                }

                #[test]
                fn records_nothing() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    f.report("s1", p.host.clone());
                    f.touch("s2", Some(p.host.clone()));
                    assert_eq!(f.sessions.hosts_of("s2"), Vec::<Host>::new());
                    assert_eq!(f.sessions.hosts_of("s1"), vec![p.host.clone()]);
                }
            }

            mod on_no_host_given {
                use super::*;

                #[test]
                fn returns_not_hosted() {
                    let mut f = fixture();
                    assert_eq!(f.touch("s1", None), Hosting::NotHosted);
                }
            }

            /// Review finding 2: a leftover call of the old session after
            /// `/clear` must not take the process back.
            mod on_a_process_that_runs_another_session_now {
                use super::*;

                #[test]
                fn changes_nothing() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    f.held_by_s1(&p);
                    f.report("s2", p.host.clone());
                    assert_eq!(
                        f.touch("s1", Some(p.host.clone())),
                        Hosting::HostRunsOther("s2".into())
                    );
                    assert!(!f.sessions.is_alive("s1"));
                    assert_eq!(f.sessions.grace_ends("s1"), Some(T0 + GRACE_MS));
                    assert!(f.sessions.is_alive("s2"));
                }
            }
        }

        /// Review finding 1: two processes running one session.
        mod two_hosts {
            use super::*;

            #[test]
            fn a_session_end_from_one_keeps_the_session_for_the_other() {
                let mut f = fixture();
                let p = Proc::spawn();
                let q = Proc::spawn();
                let lease = f.held_by_s1(&p);
                f.report("s1", q.host.clone());
                f.report("s1", p.host.clone());
                assert!(!f.end("s1", Some(p.host.clone()), "prompt_input_exit"));
                assert_eq!(f.lease(&lease).0, "held");
                assert!(f.sessions.is_alive("s1"));
            }

            #[test]
            fn one_dying_keeps_the_session_alive_for_the_other() {
                let mut f = fixture();
                let mut p = Proc::spawn();
                let q = Proc::spawn();
                f.held_by_s1(&p);
                f.report("s1", q.host.clone());
                p.kill();
                assert_eq!(f.probe(), Vec::<String>::new());
                assert!(f.sessions.is_alive("s1"));
                assert_eq!(f.sessions.grace_ends("s1"), None);
            }

            #[test]
            fn the_last_one_ending_the_session_ends_it() {
                let mut f = fixture();
                let p = Proc::spawn();
                let q = Proc::spawn();
                let lease = f.held_by_s1(&p);
                f.report("s1", q.host.clone());
                assert!(!f.end("s1", Some(p.host.clone()), "prompt_input_exit"));
                assert!(f.end("s1", Some(q.host.clone()), "prompt_input_exit"));
                assert_eq!(f.lease(&lease).0, "ended");
            }
        }

        mod probe {
            use super::*;

            mod on_the_host_process_running {
                use super::*;

                #[test]
                fn the_session_stays_alive() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    f.held_by_s1(&p);
                    assert_eq!(f.probe(), Vec::<String>::new());
                    assert!(f.sessions.is_alive("s1"));
                    assert_eq!(f.sessions.grace_ends("s1"), None);
                }
            }

            mod on_the_host_process_gone {
                use super::*;

                #[test]
                fn the_grace_starts() {
                    let mut f = fixture();
                    let mut p = Proc::spawn();
                    let lease = f.held_by_s1(&p);
                    p.kill();
                    assert_eq!(f.probe(), vec!["s1".to_owned()]);
                    assert!(!f.sessions.is_alive("s1"));
                    assert_eq!(f.sessions.grace_ends("s1"), Some(T0 + GRACE_MS));
                    assert_eq!(f.lease(&lease).0, "held");
                }
            }
        }

        mod sweep_hostless {
            use super::*;

            mod on_an_agent_holder_with_live_leases_and_no_known_host {
                use super::*;

                #[test]
                fn starts_the_reconnect_grace() {
                    let mut f = fixture();
                    f.grant(agent("ghost"));
                    assert_eq!(f.sweep(), vec!["ghost".to_owned()]);
                    assert_eq!(f.sessions.grace_ends("ghost"), Some(T0 + GRACE_MS));
                }

                #[test]
                fn ends_its_leases_when_the_grace_runs_out() {
                    let mut f = fixture();
                    let lease = f.grant(agent("ghost"));
                    f.sweep();
                    f.at(T0 + GRACE_MS);
                    assert_eq!(f.expire(), vec!["ghost".to_owned()]);
                    let (state, cause, _) = f.lease(&lease);
                    assert_eq!(state, "ended");
                    assert_eq!(cause.as_deref(), Some("holder_gone"));
                }

                #[test]
                fn a_later_sweep_does_not_extend_the_grace() {
                    let mut f = fixture();
                    f.grant(agent("ghost"));
                    f.sweep();
                    f.at(T0 + 1_000);
                    assert_eq!(f.sweep(), Vec::<String>::new());
                    assert_eq!(f.sessions.grace_ends("ghost"), Some(T0 + GRACE_MS));
                }
            }

            mod on_such_a_holder_that_reports_a_host_within_the_grace {
                use super::*;

                #[test]
                fn keeps_its_leases() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    let lease = f.grant(agent("ghost"));
                    f.sweep();
                    f.at(T0 + GRACE_MS - 1);
                    f.report("ghost", p.host.clone());
                    f.at(T0 + GRACE_MS);
                    assert_eq!(f.expire(), Vec::<String>::new());
                    assert_eq!(f.sweep(), Vec::<String>::new());
                    assert_eq!(f.lease(&lease).0, "held");
                }
            }

            mod on_a_hosted_holder_or_another_kind_of_holder {
                use super::*;

                #[test]
                fn starts_no_grace() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    f.report("s1", p.host.clone());
                    f.grant(agent("s1"));
                    assert_eq!(f.sweep(), Vec::<String>::new());
                    assert_eq!(f.sessions.grace_ends("s1"), None);
                }
            }
        }

        mod expire {
            use super::*;

            mod on_the_grace_ending {
                use super::*;

                #[test]
                fn leases_end_holder_gone_and_the_waiter_is_offered_the_unit() {
                    let mut f = fixture();
                    let mut p = Proc::spawn();
                    let lease = f.held_by_s1(&p);
                    p.kill();
                    f.probe();
                    f.at(T0 + GRACE_MS);
                    assert_eq!(f.expire(), vec!["s1".to_owned()]);
                    let (state, cause, _) = f.lease(&lease);
                    assert_eq!(state, "ended");
                    assert_eq!(cause.as_deref(), Some("holder_gone"));
                    assert!(f.offered_to(&user("w")));
                }

                #[test]
                fn its_queued_requests_drop() {
                    let mut f = fixture();
                    let other = Proc::spawn();
                    let mut p = Proc::spawn();
                    f.report("s0", other.host.clone());
                    f.grant(agent("s0"));
                    f.report("s1", p.host.clone());
                    f.queue(agent("s1"));
                    p.kill();
                    f.probe();
                    f.at(T0 + GRACE_MS);
                    f.expire();
                    assert_eq!(f.queued(), Vec::<HolderRef>::new());
                }
            }
        }

        mod boot {
            use super::*;

            mod on_a_session_whose_recorded_host_still_runs {
                use super::*;

                #[test]
                fn stays_alive_with_no_grace() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    f.held_by_s1(&p);
                    assert_eq!(f.restart(), Vec::<String>::new());
                    assert!(f.sessions.is_alive("s1"));
                    assert_eq!(f.sessions.host_of("s1"), Some(p.host.clone()));
                }
            }

            mod on_a_session_whose_recorded_host_is_gone {
                use super::*;

                #[test]
                fn gets_the_grace_from_boot_and_a_report_within_it_keeps_its_leases() {
                    let mut f = fixture();
                    let mut p = Proc::spawn();
                    let lease = f.held_by_s1(&p);
                    p.kill();
                    f.at(T0 + 10);
                    assert_eq!(f.restart(), vec!["s1".to_owned()]);
                    assert_eq!(f.sessions.grace_ends("s1"), Some(T0 + 10 + GRACE_MS));
                    let new = Proc::spawn();
                    f.report("s1", new.host.clone());
                    f.at(T0 + 3 * GRACE_MS);
                    assert_eq!(f.expire(), Vec::<String>::new());
                    assert_eq!(f.lease(&lease).0, "held");
                }
            }

            mod on_a_session_with_no_recorded_host {
                use super::*;

                #[test]
                fn ends_after_the_grace() {
                    let mut f = fixture();
                    let lease = f.grant(agent("mcp-era"));
                    assert_eq!(f.restart(), vec!["mcp-era".to_owned()]);
                    f.at(T0 + GRACE_MS);
                    assert_eq!(f.expire(), vec!["mcp-era".to_owned()]);
                    assert_eq!(f.lease(&lease).0, "ended");
                }
            }

            mod on_a_session_unhosted_before_the_restart {
                use super::*;

                #[test]
                fn gets_the_grace_even_though_its_old_host_still_runs() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    f.held_by_s1(&p);
                    f.report("s2", p.host.clone());
                    assert_eq!(f.restart(), vec!["s1".to_owned()]);
                    assert!(!f.sessions.is_alive("s1"));
                    assert!(f.sessions.is_alive("s2"));
                }
            }
        }

        mod end {
            use super::*;

            mod on_any_reason_from_the_sessions_current_host {
                use super::*;

                #[test]
                fn leases_end_at_once_with_cleanup_not_confirmed() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    let lease = f.held_by_s1(&p);
                    assert!(f.end("s1", Some(p.host.clone()), "clear"));
                    let (state, cause, _) = f.lease(&lease);
                    assert_eq!(state, "ended");
                    assert_eq!(cause.as_deref(), Some("holder_gone"));
                    assert!(f.offered_to(&user("w")));
                }

                #[test]
                fn audits_session_ended_with_the_reason() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    f.held_by_s1(&p);
                    f.end("s1", Some(p.host.clone()), "prompt_input_exit");
                    let entries = f.audit("session_ended");
                    assert_eq!(entries.len(), 1);
                    assert_eq!(entries[0].detail["session"], "s1");
                    assert_eq!(entries[0].detail["reason"], "prompt_input_exit");
                }

                #[test]
                fn forgets_the_session() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    f.held_by_s1(&p);
                    f.end("s1", Some(p.host.clone()), "logout");
                    assert_eq!(f.sessions.host_of("s1"), None);
                    assert_eq!(f.sessions.grace_ends("s1"), None);
                }
            }

            mod from_a_process_that_is_not_the_current_host {
                use super::*;

                #[test]
                fn changes_nothing() {
                    let mut f = fixture();
                    let p = Proc::spawn();
                    let other = Proc::spawn();
                    let lease = f.held_by_s1(&p);
                    assert!(!f.end("s1", Some(other.host.clone()), "prompt_input_exit"));
                    assert_eq!(f.lease(&lease).0, "held");
                    assert!(f.sessions.is_alive("s1"));
                    assert_eq!(f.audit("session_ended").len(), 0);
                }
            }

            mod on_a_session_its_process_already_left {
                use super::*;

                #[test]
                fn ends_it_at_once() {
                    // `/clear`: SessionStart of the new session may report
                    // before SessionEnd of the old one lands.
                    let mut f = fixture();
                    let p = Proc::spawn();
                    let lease = f.held_by_s1(&p);
                    f.report("s2", p.host.clone());
                    assert!(f.end("s1", Some(p.host.clone()), "clear"));
                    assert_eq!(f.lease(&lease).0, "ended");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// engine wiring
// ---------------------------------------------------------------------------

/// An engine on `store` with a manual clock and a fast probe loop.
fn engine_on(store: Store, clock: Arc<ManualClock>) -> EngineHandle {
    Engine::spawn_with(
        store,
        clock,
        Config::default(),
        Arc::new(Healthy),
        EngineOptions {
            liveness_interval: Duration::from_millis(10),
        },
    )
    .unwrap()
}

/// A seeded store and its clock.
fn seeded() -> (Store, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(T0));
    let mut store = Store::open_in_memory().unwrap();
    seed(&mut store, &Inventory::new(clock.clone()));
    (store, clock)
}

/// Run `method` as `holder`, reporting `host`.
async fn rpc(
    engine: &EngineHandle,
    method: &str,
    holder: HolderRef,
    host: Option<Host>,
    args: serde_json::Value,
) -> Result<serde_json::Value, ApiError> {
    let command = EngineCommand::parse(method, as_caller(holder), args)?;
    engine.call_from(command, host).await
}

fn acquire_args() -> serde_json::Value {
    serde_json::json!({
        "target": {"resource": "board"}, "duration_ms": 3_600_000, "purpose": "test"
    })
}

async fn acquire(
    engine: &EngineHandle,
    holder: HolderRef,
    host: Option<Host>,
) -> Result<serde_json::Value, ApiError> {
    rpc(engine, "acquire", holder, host, acquire_args()).await
}

/// How many live leases and queued requests `session` has.
async fn holdings(engine: &EngineHandle, session: &str) -> usize {
    let mine = rpc(
        engine,
        "list_my_leases",
        agent(session),
        None,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    mine["leases"].as_array().unwrap().len() + mine["requests"].as_array().unwrap().len()
}

/// `board/u1` held by a user; the lease id.
async fn user_holds(engine: &EngineHandle) -> String {
    let lease = acquire(engine, user("x"), None).await.unwrap();
    lease["lease_id"].as_str().unwrap().to_owned()
}

/// `session`, on `host`, is offered `board/u1`; the lease id.
async fn offered(engine: &EngineHandle, session: &str, host: Host) -> String {
    let held = user_holds(engine).await;
    let queued = acquire(engine, agent(session), Some(host)).await.unwrap();
    assert_eq!(queued["outcome"], "queued", "{queued}");
    let lease = serde_json::json!({ "lease_id": held });
    rpc(engine, "release", user("x"), None, lease.clone())
        .await
        .unwrap();
    rpc(engine, "confirm", user("x"), None, lease)
        .await
        .unwrap();
    let got = engine
        .wait_notices(agent(session), Duration::from_secs(30))
        .await
        .unwrap();
    got[0].lease_id.as_ref().unwrap().as_str().to_owned()
}

fn assert_not_hosted<T: std::fmt::Debug>(result: Result<T, ApiError>) {
    let error = result.unwrap_err();
    assert_eq!(error.code, "session_not_hosted", "{error:?}");
}

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !check().await {
        assert!(Instant::now() < until, "{what} never happened");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

mod engine {
    use super::*;

    /// Review finding 4: one armed wait per session, also between steps.
    mod agent_wait {
        use super::*;
        use leclerk::engine::AgentWait;

        #[tokio::test(flavor = "multi_thread")]
        async fn a_second_wait_starting_between_the_first_waits_steps_is_refused() {
            let (store, clock) = seeded();
            let engine = engine_on(store, clock);
            let p = Proc::spawn();
            let step = Duration::from_millis(100);
            let first = engine
                .agent_wait("s1".into(), p.host.clone(), step, true, "arm-a".into())
                .await
                .unwrap();
            assert_eq!(first, AgentWait::Timeout);
            let second = engine
                .agent_wait("s1".into(), p.host.clone(), step, true, "arm-b".into())
                .await
                .unwrap();
            assert_eq!(second, AgentWait::Armed);
            let again = engine
                .agent_wait("s1".into(), p.host.clone(), step, false, "arm-a".into())
                .await
                .unwrap();
            assert_eq!(again, AgentWait::Timeout);
        }

        /// Research finding 1: a wait that is stopped (Monitor expiry)
        /// gives its arm up, so the next wait arms at once.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_disarmed_wait_lets_the_next_one_arm_at_once() {
            let (store, clock) = seeded();
            let engine = engine_on(store, clock);
            let p = Proc::spawn();
            let step = Duration::from_millis(100);
            engine
                .agent_wait("s1".into(), p.host.clone(), step, true, "arm-a".into())
                .await
                .unwrap();
            engine.disarm("s1".into(), "arm-a");
            let next = engine
                .agent_wait("s1".into(), p.host.clone(), step, true, "arm-b".into())
                .await
                .unwrap();
            assert_eq!(next, AgentWait::Timeout);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn disarming_with_another_token_changes_nothing() {
            let (store, clock) = seeded();
            let engine = engine_on(store, clock);
            let p = Proc::spawn();
            let step = Duration::from_millis(100);
            engine
                .agent_wait("s1".into(), p.host.clone(), step, true, "arm-a".into())
                .await
                .unwrap();
            engine.disarm("s1".into(), "arm-x");
            let next = engine
                .agent_wait("s1".into(), p.host.clone(), step, true, "arm-b".into())
                .await
                .unwrap();
            assert_eq!(next, AgentWait::Armed);
        }
    }

    /// AR-H1: the daemon, not only the CLI, ties an agent's lease to a
    /// running process.
    mod command_validation {
        use super::*;

        mod acquire_command {
            use super::*;

            mod on_an_agent_session_that_is_not_hosted {
                use super::*;

                #[tokio::test(flavor = "multi_thread")]
                async fn refuses_with_session_not_hosted() {
                    let (store, clock) = seeded();
                    let engine = engine_on(store, clock);
                    assert_not_hosted(acquire(&engine, agent("s1"), None).await);
                }

                #[tokio::test(flavor = "multi_thread")]
                async fn refuses_a_host_running_another_session() {
                    let (store, clock) = seeded();
                    let engine = engine_on(store, clock);
                    let p = Proc::spawn();
                    engine
                        .report_host("s1".into(), p.host.clone())
                        .await
                        .unwrap();
                    assert_not_hosted(acquire(&engine, agent("s2"), Some(p.host.clone())).await);
                }

                #[tokio::test(flavor = "multi_thread")]
                async fn refuses_a_call_with_no_host_though_the_session_has_one() {
                    let (store, clock) = seeded();
                    let engine = engine_on(store, clock);
                    let p = Proc::spawn();
                    engine
                        .report_host("s1".into(), p.host.clone())
                        .await
                        .unwrap();
                    assert_not_hosted(acquire(&engine, agent("s1"), None).await);
                }

                #[tokio::test(flavor = "multi_thread")]
                async fn grants_nothing() {
                    let (store, clock) = seeded();
                    let engine = engine_on(store, clock);
                    let _ = acquire(&engine, agent("s1"), None).await;
                    assert_eq!(holdings(&engine, "s1").await, 0);
                }
            }

            mod on_a_hosted_agent_session {
                use super::*;

                #[tokio::test(flavor = "multi_thread")]
                async fn proceeds_as_before() {
                    let (store, clock) = seeded();
                    let engine = engine_on(store, clock);
                    let p = Proc::spawn();
                    let got = acquire(&engine, agent("s1"), Some(p.host.clone()))
                        .await
                        .unwrap();
                    assert_eq!(got["outcome"], "granted", "{got}");
                    assert!(engine.session_alive("s1".into()).await.unwrap());
                }
            }

            mod on_a_caller_that_is_not_an_agent_session {
                use super::*;

                #[tokio::test(flavor = "multi_thread")]
                async fn needs_no_host() {
                    let (store, clock) = seeded();
                    let engine = engine_on(store, clock);
                    let got = acquire(&engine, user("x"), None).await.unwrap();
                    assert_eq!(got["outcome"], "granted", "{got}");
                }
            }
        }

        mod claim_command {
            use super::*;

            mod on_an_agent_session_that_is_not_hosted {
                use super::*;

                #[tokio::test(flavor = "multi_thread")]
                async fn refuses_with_session_not_hosted_and_claims_nothing() {
                    let (store, clock) = seeded();
                    let engine = engine_on(store, clock);
                    let p = Proc::spawn();
                    let lease = offered(&engine, "s1", p.host.clone()).await;
                    let args = serde_json::json!({ "lease_id": lease });
                    assert_not_hosted(rpc(&engine, "claim", agent("s1"), None, args.clone()).await);
                    let view = rpc(&engine, "get_lease", agent("s1"), None, args)
                        .await
                        .unwrap();
                    assert_eq!(view["state"], "offered", "{view}");
                }
            }

            mod on_a_hosted_agent_session {
                use super::*;

                #[tokio::test(flavor = "multi_thread")]
                async fn proceeds_as_before() {
                    let (store, clock) = seeded();
                    let engine = engine_on(store, clock);
                    let p = Proc::spawn();
                    let lease = offered(&engine, "s1", p.host.clone()).await;
                    let args = serde_json::json!({ "lease_id": lease });
                    let view = rpc(&engine, "claim", agent("s1"), Some(p.host.clone()), args)
                        .await
                        .unwrap();
                    assert_eq!(view["state"], "held", "{view}");
                }
            }
        }

        mod rehome_holder {
            use super::*;

            fn to(lease: &str, session: &str) -> serde_json::Value {
                serde_json::json!({ "lease_id": lease, "new_holder": agent(session) })
            }

            mod on_a_target_agent_session_that_is_not_hosted {
                use super::*;

                #[tokio::test(flavor = "multi_thread")]
                async fn refuses_with_session_not_hosted_and_moves_nothing() {
                    let (store, clock) = seeded();
                    let engine = engine_on(store, clock);
                    let lease = user_holds(&engine).await;
                    let args = to(&lease, "ghost");
                    assert_not_hosted(rpc(&engine, "rehome_holder", user("x"), None, args).await);
                    assert_eq!(holdings(&engine, "ghost").await, 0);
                }
            }

            mod on_a_hosted_target_agent_session {
                use super::*;

                #[tokio::test(flavor = "multi_thread")]
                async fn moves_the_lease() {
                    let (store, clock) = seeded();
                    let engine = engine_on(store, clock);
                    let p = Proc::spawn();
                    engine
                        .report_host("s1".into(), p.host.clone())
                        .await
                        .unwrap();
                    let lease = user_holds(&engine).await;
                    rpc(&engine, "rehome_holder", user("x"), None, to(&lease, "s1"))
                        .await
                        .unwrap();
                    assert_eq!(holdings(&engine, "s1").await, 1);
                }
            }

            mod on_a_caller_agent_session_that_is_not_hosted {
                use super::*;

                #[tokio::test(flavor = "multi_thread")]
                async fn refuses_with_session_not_hosted() {
                    let (store, clock) = seeded();
                    let engine = engine_on(store, clock);
                    let p = Proc::spawn();
                    let lease = acquire(&engine, agent("s1"), Some(p.host.clone()))
                        .await
                        .unwrap();
                    let args = serde_json::json!({
                        "lease_id": lease["lease_id"], "new_holder": user("y"),
                    });
                    // An agent may rehome only on the user's word: give it, so
                    // the hosting check is what answers.
                    let caller = Caller {
                        override_reason: Some("the user asked".into()),
                        ..as_caller(agent("s1"))
                    };
                    let command = EngineCommand::parse("rehome_holder", caller, args).unwrap();
                    assert_not_hosted(engine.call_from(command, None).await);
                }
            }
        }
    }

    mod sweep_on_the_probe_tick {
        use super::*;

        /// A lease that appears after boot with no host on record: only the
        /// probe's sweep can see it.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_hostless_holder_first_seen_after_boot_gets_the_grace_and_its_lease_ends() {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("clerk.db");
            let clock = Arc::new(ManualClock::new(T0));
            let mut store = Store::open(&path).unwrap();
            seed(&mut store, &Inventory::new(clock.clone()));
            let engine = engine_on(store, clock.clone());
            let mut outside = Store::open(&path).unwrap();
            let sched = Scheduler::new(clock.clone(), Config::default(), Arc::new(Healthy));
            let who = as_caller(agent("ghost"));
            let lease = match outside.tx(|tx| sched.acquire(tx, &who, req())).unwrap() {
                AcquireOutcome::Granted(g) => g.lease_id,
                other => panic!("expected grant, got {other:?}"),
            };
            let row = |store: &mut Store| {
                store
                    .tx(|tx| -> Result<_, StoreError> { Ok(lease_row(tx, &lease)) })
                    .unwrap()
            };
            // The sweep may not have run yet when the clock moves, so it
            // moves in steps of one grace, far short of the lease's end.
            eventually("the lease ends", async || {
                clock.advance(GRACE);
                tokio::time::sleep(Duration::from_millis(50)).await;
                row(&mut outside).0 == "ended"
            })
            .await;
            assert_eq!(row(&mut outside).1.as_deref(), Some("holder_gone"));
            drop(engine);
        }
    }

    mod report_host {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn makes_the_session_alive() {
            let (store, clock) = seeded();
            let engine = engine_on(store, clock);
            let p = Proc::spawn();
            engine
                .report_host("s1".into(), p.host.clone())
                .await
                .unwrap();
            assert!(engine.session_alive("s1".into()).await.unwrap());
        }
    }

    mod probe {
        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn a_dead_host_starts_the_grace_and_the_clock_ends_the_lease() {
            let (store, clock) = seeded();
            let engine = engine_on(store, clock.clone());
            let mut p = Proc::spawn();
            engine
                .report_host("s1".into(), p.host.clone())
                .await
                .unwrap();
            acquire(&engine, agent("s1"), Some(p.host.clone()))
                .await
                .unwrap();
            p.kill();
            eventually("the probe notices the dead host", async || {
                !engine.session_alive("s1".into()).await.unwrap()
            })
            .await;
            clock.set(T0 + GRACE_MS);
            eventually("the lease ends", async || {
                let me = as_caller(agent("s1"));
                let mine = engine
                    .call(
                        leclerk::engine::Command::parse(
                            "list_my_leases",
                            me,
                            serde_json::json!({}),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                mine["leases"].as_array().unwrap().is_empty()
            })
            .await;
        }
    }
}
