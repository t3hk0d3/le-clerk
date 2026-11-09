//! The interactive leasing view, driven through a pseudo-terminal: each
//! test runs `clerk` under `script -qefc` and types `\x03` (Ctrl+C) or
//! `\x1d` (Ctrl+]) into it.

mod common;

use std::fs;

use leclerk::model::{Caller, HolderRef};
use leclerk::scope::ProjectResolver;
use serde_json::{Value, json};

use common::{CTRL_BRACKET, CTRL_C, Env, PATIENCE, SESSION_VAR, Tty};

fn user(tag: &str) -> Caller {
    Caller {
        holder: HolderRef::User { tag: tag.into() },
        project: None,
        override_reason: None,
    }
}

/// The board: one unit `u1` keyed `k1`, tags and a usage template.
fn seed(env: &Env) {
    env.ok(&[
        "resource",
        "create",
        "board",
        "--type",
        "generic",
        "--description",
        "A test board",
        "--tag",
        "esp32,thread",
        "--usage",
        "Flash with key {unit.identity_key}",
    ]);
    env.ok(&[
        "unit",
        "add",
        "board",
        "u1",
        "--key",
        "k1",
        "--description",
        "The left one",
    ]);
}

/// User `who` holds the board's unit; returns the lease id.
fn hold(env: &Env, who: &str) -> String {
    let grant = env
        .rpc(
            "acquire",
            &user(who),
            json!({"target": {"resource": "board"}, "duration_ms": 600_000, "purpose": "theirs"}),
        )
        .unwrap();
    assert_eq!(grant["outcome"], "granted", "{grant}");
    grant["lease_id"].as_str().unwrap().to_owned()
}

/// User `who` releases `lease` and confirms its cleanup.
fn free(env: &Env, who: &str, lease: &str) {
    env.rpc("release", &user(who), json!({ "lease_id": lease }))
        .unwrap();
    env.rpc("confirm", &user(who), json!({ "lease_id": lease }))
        .unwrap();
}

/// An agent takes the board regardless: its holder gets `Overridden`.
fn override_board(env: &Env) {
    let dir = env.dir("alpha");
    let caller = Caller {
        holder: HolderRef::AgentSession {
            session_id: "sess-1".into(),
        },
        project: Some(ProjectResolver::resolve(&dir).unwrap()),
        override_reason: Some("the user asked".into()),
    };
    env.rpc(
        "acquire",
        &caller,
        json!({
            "target": {"resource": "board"},
            "duration_ms": 60_000,
            "purpose": "take it",
            "override_reason": "the user asked",
        }),
    )
    .unwrap();
}

/// The board unit's entry in the status view.
fn unit_status(env: &Env) -> Value {
    env.ok(&["status"])["resources"][0]["units"][0].clone()
}

/// Audit entries of `action`, newest first.
fn audit(env: &Env, action: &str) -> Vec<Value> {
    env.ok(&["log", "--action", action])
        .as_array()
        .unwrap()
        .clone()
}

/// The `lease_ended` audit detail of `lease`.
fn ending(env: &Env, lease: &str) -> Value {
    audit(env, "lease_ended")
        .into_iter()
        .map(|e| e["detail"].clone())
        .find(|d| d["lease"] == lease)
        .unwrap_or_else(|| panic!("lease {lease} never ended"))
}

/// A user lease on the board, acquired without a terminal.
fn detached_lease(env: &Env) -> String {
    env.ok(&["acquire", "board", "--for", "10m"])["lease_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

const ACQUIRE: &str = "\"$CLERK\" acquire board --for 10m";

/// A user request for the board, made without a terminal while the unit
/// is held (exit 6).
fn queue_detached(env: &Env) {
    let out = env
        .env
        .cmd()
        .env_remove(SESSION_VAR)
        .args(["acquire", "board", "--for", "10m"])
        .timeout(PATIENCE)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(6), "{out:?}");
}

// ---------------------------------------------------------------------------
// Behaviour
// ---------------------------------------------------------------------------

mod cli {
    use super::*;

    mod acquire {
        use super::*;

        mod on_stdin_tty {
            use super::*;

            #[test]
            fn shows_queue_position_then_grant_then_notices() {
                let env = Env::new();
                seed(&env);
                let theirs = hold(&env, "x");
                let mut tty = Tty::spawn(&env, ACQUIRE);
                tty.expect("position 1");
                free(&env, "x", &theirs);
                tty.expect("Granted");
                override_board(&env);
                tty.expect("overridden");
                tty.send(CTRL_C);
                let (_, shown) = tty.finish();
                let at = |s: &str| shown.find(s).unwrap();
                assert!(at("position 1") < at("Granted"), "{shown}");
                assert!(at("Granted") < at("overridden"), "{shown}");
            }

            #[test]
            fn ctrl_c_while_queued_cancels() {
                let env = Env::new();
                seed(&env);
                hold(&env, "x");
                let mut tty = Tty::spawn(&env, ACQUIRE);
                tty.expect("position 1");
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                assert_eq!(code, 130, "{shown}");
                assert_eq!(env.ok(&["queue", "board"])[0]["queue"], json!([]));
                assert_eq!(audit(&env, "request_cancelled").len(), 1);
            }

            #[test]
            fn ctrl_c_while_held_releases_with_confirm_clean() {
                let env = Env::new();
                seed(&env);
                let mut tty = Tty::spawn(&env, ACQUIRE);
                tty.expect("Granted");
                let lease = unit_status(&env)["occupancy"]["lease_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                assert_eq!(code, 130, "{shown}");
                let ended = ending(&env, &lease);
                assert_eq!(ended["cause"], "released");
                assert_eq!(ended["cleanup_confirmed"], true);
                assert_eq!(unit_status(&env)["dirty"], Value::Null);
            }

            #[test]
            fn ctrl_bracket_detaches_lease_kept_holder_user_prints_lease_id() {
                let env = Env::new();
                seed(&env);
                let mut tty = Tty::spawn(&env, ACQUIRE);
                tty.expect("Granted");
                tty.send(CTRL_BRACKET);
                let (code, shown) = tty.finish();
                assert_eq!(code, 0, "{shown}");
                let occupancy = unit_status(&env)["occupancy"].clone();
                assert_eq!(occupancy["state"], "held", "{occupancy}");
                assert_eq!(occupancy["holder"]["kind"], "user", "{occupancy}");
                let lease = occupancy["lease_id"].as_str().unwrap();
                let after_grant = &shown[shown.find("Granted").unwrap()..];
                assert!(after_grant.contains(lease), "{shown}");
            }

            mod on_daemon_restart_mid_view {
                use super::*;

                /// A held live view whose daemon was just restarted;
                /// returns the view and its lease.
                fn restarted(env: &Env) -> (Tty, String) {
                    seed(env);
                    let tty = Tty::spawn(env, ACQUIRE);
                    tty.expect("Granted");
                    let lease = unit_status(env)["occupancy"]["lease_id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    env.ok(&["daemon", "restart"]);
                    (tty, lease)
                }

                #[test]
                fn keeps_the_lease() {
                    let env = Env::new();
                    let (_tty, lease) = restarted(&env);
                    // Past the new daemon's first liveness probes.
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    let occupancy = unit_status(&env)["occupancy"].clone();
                    assert_eq!(occupancy["lease_id"], lease.as_str(), "{occupancy}");
                    assert_eq!(occupancy["state"], "held", "{occupancy}");
                    assert_eq!(occupancy["holder"]["kind"], "cli_process", "{occupancy}");
                }

                #[test]
                fn says_it_lost_contact() {
                    let env = Env::new();
                    let (tty, _) = restarted(&env);
                    tty.expect("Cannot reach the clerk daemon");
                }

                #[test]
                fn still_shows_notices() {
                    let env = Env::new();
                    let (tty, _) = restarted(&env);
                    tty.expect("Cannot reach the clerk daemon");
                    override_board(&env);
                    tty.expect("overridden");
                }
            }

            #[test]
            fn terminal_restored_to_cooked_mode_on_sigterm() {
                let env = Env::new();
                seed(&env);
                let tty = Tty::spawn(&env, &format!("{ACQUIRE}; echo AFTER; stty -a"));
                tty.expect("Granted");
                let holder = unit_status(&env)["occupancy"]["holder"].clone();
                let pid = libc::pid_t::try_from(holder["pid"].as_u64().unwrap()).unwrap();
                // SAFETY: plain kill(2) on the clerk process the test started.
                assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
                let (_, shown) = tty.finish();
                let after = &shown[shown.find("AFTER").expect("clerk ended")..];
                assert!(after.contains(" icanon"), "{after}");
                assert!(!after.contains("-icanon"), "{after}");
            }
        }

        mod on_grant {
            use super::*;

            #[test]
            fn prints_brief() {
                let env = Env::new();
                seed(&env);
                let mut tty = Tty::spawn(&env, ACQUIRE);
                tty.expect("Flash with key k1");
                tty.send(CTRL_C);
                let (_, shown) = tty.finish();
                for part in ["A test board", "The left one", "esp32, thread"] {
                    assert!(shown.contains(part), "{part}: {shown}");
                }
            }
        }

        mod on_ctrl_c_with_offer_pending {
            use super::*;

            /// Queue the CLI behind user x, free the unit so the CLI is
            /// offered it, and press Ctrl+C while its claim waits. Returns
            /// the exit code, what was shown and the offered lease's id.
            fn interrupt_pending_claim(env: &Env) -> (i32, String, String) {
                seed(env);
                let theirs = hold(env, "x");
                let mut tty =
                    Tty::spawn_with(env, ACQUIRE, &[("CLERK_TEST_CLAIM_DELAY_MS", "60000")]);
                tty.expect("position 1");
                free(env, "x", &theirs);
                tty.expect("claiming");
                let occupancy = unit_status(env)["occupancy"].clone();
                assert_eq!(occupancy["state"], "offered", "{occupancy}");
                let offer = occupancy["lease_id"].as_str().unwrap().to_owned();
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                (code, shown, offer)
            }

            #[test]
            fn offer_withdrawn_nothing_held() {
                let env = Env::new();
                let (_, shown, offer) = interrupt_pending_claim(&env);
                let cancelled = &audit(&env, "request_cancelled")[0]["detail"];
                assert_eq!(cancelled["offer"], offer.as_str(), "{shown}");
                assert_eq!(ending(&env, &offer)["cause"], "unclaimed");
                assert_eq!(audit(&env, "claimed"), Vec::<Value>::new());
                assert_eq!(audit(&env, "offer_declined"), Vec::<Value>::new());
                assert_eq!(unit_status(&env)["occupancy"]["kind"], "free");
            }

            #[test]
            fn exits_130() {
                let env = Env::new();
                let (code, shown, _) = interrupt_pending_claim(&env);
                assert_eq!(code, 130, "{shown}");
            }
        }
    }

    mod run {
        use super::*;

        mod on_ctrl_c {
            use super::*;

            #[test]
            fn cmd_gets_one_sigint() {
                let env = Env::new();
                seed(&env);
                let file = env.home().join("ints");
                let file_text = file.to_string_lossy().into_owned();
                let mut tty = Tty::spawn_with(
                    &env,
                    "\"$CLERK\" run board --for 10m -- sh -c 'trap \"echo INT >> \\\"$F\\\"\" INT; \
                     echo started; i=0; while [ $i -lt 20 ]; do sleep 0.1; i=$((i+1)); done'",
                    &[("F", &file_text)],
                );
                tty.expect("started");
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                assert_eq!(code, 0, "{shown}");
                let ints = fs::read_to_string(&file).unwrap_or_default();
                assert_eq!(ints.lines().count(), 1, "{ints:?}\n{shown}");
                // Near-simultaneous SIGINTs can merge into one trap run, so
                // the count alone cannot see a forwarded copy; clerk says
                // when it forwards one.
                assert!(!shown.contains("Passing SIGINT on"), "{shown}");
            }
        }
    }

    mod attach {
        use super::*;

        mod on_offered_lease {
            use super::*;

            #[test]
            fn ctrl_c_cancels_the_request_withdrawing_the_offer() {
                let env = Env::new();
                seed(&env);
                let theirs = hold(&env, "x");
                queue_detached(&env);
                free(&env, "x", &theirs);
                let occupancy = unit_status(&env)["occupancy"].clone();
                assert_eq!(occupancy["state"], "offered", "{occupancy}");
                let offer = occupancy["lease_id"].as_str().unwrap().to_owned();
                let mut tty = Tty::spawn_with(
                    &env,
                    &format!("\"$CLERK\" attach {offer}"),
                    &[("CLERK_TEST_CLAIM_DELAY_MS", "60000")],
                );
                tty.expect("claiming");
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                assert_eq!(code, 130, "{shown}");
                let cancelled = &audit(&env, "request_cancelled")[0]["detail"];
                assert_eq!(cancelled["offer"], offer.as_str(), "{shown}");
                assert_eq!(ending(&env, &offer)["cause"], "unclaimed");
                assert_eq!(audit(&env, "offer_declined"), Vec::<Value>::new());
            }
        }

        mod on_tty {
            use super::*;

            #[test]
            fn prints_brief_then_shows_notices_live() {
                let env = Env::new();
                seed(&env);
                let lease = detached_lease(&env);
                let mut tty = Tty::spawn(&env, &format!("\"$CLERK\" attach {lease}"));
                tty.expect("Flash with key k1");
                override_board(&env);
                tty.expect("overridden");
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                assert_eq!(code, 130, "{shown}");
                for part in ["A test board", "The left one", "esp32, thread"] {
                    assert!(shown.contains(part), "{part}: {shown}");
                }
            }

            #[test]
            fn ctrl_c_releases() {
                let env = Env::new();
                seed(&env);
                let lease = detached_lease(&env);
                let mut tty = Tty::spawn(&env, &format!("\"$CLERK\" attach {lease}"));
                tty.expect("Flash with key k1");
                tty.send(CTRL_C);
                let (code, shown) = tty.finish();
                assert_eq!(code, 130, "{shown}");
                let ended = ending(&env, &lease);
                assert_eq!(ended["cause"], "released");
                assert_eq!(ended["cleanup_confirmed"], true);
            }

            #[test]
            fn ctrl_bracket_detaches_again() {
                let env = Env::new();
                seed(&env);
                let lease = detached_lease(&env);
                let before = unit_status(&env)["occupancy"].clone();
                let mut tty = Tty::spawn(&env, &format!("\"$CLERK\" attach {lease}"));
                tty.expect("Flash with key k1");
                tty.send(CTRL_BRACKET);
                let (code, shown) = tty.finish();
                assert_eq!(code, 0, "{shown}");
                let after = unit_status(&env)["occupancy"].clone();
                assert_eq!(after["lease_id"], lease.as_str());
                assert_eq!(after["state"], "held");
                assert_eq!(after["holder"], before["holder"]);
            }
        }
    }
}
