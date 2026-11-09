//! The interactive live view of `acquire` and `attach`: raw mode, keys and
//! the loop over notices.
//!
//! Raw mode is entered only when stdin is a terminal, and is left on every
//! exit path: [`RawMode`] restores the terminal when dropped, a panic hook
//! restores it before the panic message, and SIGTERM, SIGHUP and SIGINT end
//! the view (cancelling or releasing what it holds) before the process
//! exits. In raw mode Ctrl+C (`0x03`) and Ctrl+] (`0x1d`) arrive as bytes.

use std::cell::RefCell;
use std::future::Future;
use std::io::{self, IsTerminal, Read, Write};
use std::pin::Pin;
use std::sync::Once;
use std::time::Duration;

use crossterm::terminal;
use leclerk::client::{ApiError, Client};
use leclerk::model::{Caller, HolderRef};
use leclerk::store::notices::{Notice, NoticeKind};
use serde_json::{Value, json};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use ulid::Ulid;

use super::lease;
use super::output::{EXIT_INTERRUPTED, Exit, Mode};

/// Ctrl+C: cancel the request, or release the lease.
const CTRL_C: u8 = 0x03;
/// Ctrl+]: detach, keeping the lease.
const CTRL_BRACKET: u8 = 0x1d;
/// The longest the view spends cancelling or releasing after a signal.
const SIGNAL_CLEANUP: Duration = Duration::from_secs(5);

/// Raw mode on the terminal while alive; cooked mode again when dropped.
pub struct RawMode(());

impl RawMode {
    /// Enter raw mode, installing the panic hook that leaves it.
    ///
    /// # Errors
    /// When the terminal refuses.
    pub fn enable() -> io::Result<Self> {
        static HOOK: Once = Once::new();
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let _ = terminal::disable_raw_mode();
                previous(info);
            }));
        });
        terminal::enable_raw_mode()?;
        Ok(Self(()))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Where the view prints: stdout, one JSON value or one block of text per
/// event, with `\r\n` line ends on a raw terminal.
pub struct Screen {
    mode: Mode,
    crlf: bool,
}

impl Screen {
    #[must_use]
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            crlf: io::stdout().is_terminal(),
        }
    }

    /// Print `value` (JSON mode) or `text` (human mode).
    pub fn say(&self, value: &Value, text: &str) {
        let line = match self.mode {
            Mode::Json => value.to_string(),
            Mode::Human => text.trim_end_matches('\n').to_owned(),
        };
        let line = if self.crlf {
            line.replace('\n', "\r\n") + "\r\n"
        } else {
            line + "\n"
        };
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(line.as_bytes());
        let _ = stdout.flush();
    }

    /// A line for people only: nothing in JSON mode.
    pub fn hint(&self, text: &str) {
        if self.mode == Mode::Human {
            self.say(&Value::Null, text);
        }
    }

    /// The keys that work while a lease is held.
    pub fn hint_held(&self) {
        self.hint("Ctrl+C releases the lease; Ctrl+] detaches and keeps it.");
    }

    /// Print a notice.
    pub fn notice(&self, n: &Notice) {
        let value = serde_json::to_value(n).unwrap_or(Value::Null);
        self.say(&value, &lease::notice_text(n));
    }
}

/// Where the view stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Waiting in the queue.
    Queued { request: String },
    /// Offered a unit; the claim is not answered yet.
    Offered { request: String, lease: String },
    /// Holding a lease (in any live state).
    Held { lease: String },
}

/// One long-poll for `holder`'s notices, owning what it needs so it can
/// outlive a loop iteration (dropping it would lose the notices it took).
fn poll(client: &Client, holder: &HolderRef) -> PollFuture {
    let (client, holder) = (client.clone(), holder.clone());
    Box::pin(async move { client.wait_notices(&holder, lease::POLL_SECS).await })
}

type PollFuture = Pin<Box<dyn Future<Output = Result<Vec<Notice>, ApiError>>>>;

/// Bytes typed on stdin, read by a thread of their own (the read blocks;
/// the thread ends with the process).
fn keys() -> mpsc::UnboundedReceiver<u8> {
    let (tx, rx) = mpsc::unbounded_channel();
    let spawned = std::thread::Builder::new()
        .name("clerk-keys".to_owned())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut buf = [0u8; 64];
            while let Ok(n @ 1..) = stdin.read(&mut buf) {
                for &b in &buf[..n] {
                    if tx.send(b).is_err() {
                        return;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot read keys; Ctrl+C and Ctrl+] will not work");
    }
    rx
}

/// Run the live view from `phase` until Ctrl+C (exit 130), Ctrl+] (exit 0),
/// the end of the lease (exit 0) or a signal (exit 128 + its number). Raw
/// mode must already be on.
///
/// # Errors
/// [`Exit`] for the exits above; a daemon error ends the view with it.
pub async fn run(
    client: &Client,
    caller: &Caller,
    screen: &Screen,
    phase: Phase,
) -> anyhow::Result<()> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let view = View {
        client,
        caller,
        screen,
        phase: RefCell::new(phase),
    };
    let number = tokio::select! {
        result = view.session() => return result,
        _ = terminate.recv() => libc::SIGTERM,
        _ = hangup.recv() => libc::SIGHUP,
        _ = interrupt.recv() => libc::SIGINT,
    };
    if tokio::time::timeout(SIGNAL_CLEANUP, view.abandon())
        .await
        .is_err()
    {
        tracing::warn!("gave up cancelling or releasing after a signal");
    }
    Err(Exit(u8::try_from(128 + number).unwrap_or(u8::MAX)).into())
}

struct View<'a> {
    client: &'a Client,
    caller: &'a Caller,
    screen: &'a Screen,
    phase: RefCell<Phase>,
}

impl View<'_> {
    fn phase(&self) -> Phase {
        self.phase.borrow().clone()
    }

    fn set(&self, phase: Phase) {
        *self.phase.borrow_mut() = phase;
    }

    async fn session(&self) -> anyhow::Result<()> {
        let mut keys = keys();
        let mut typing = true;
        let mut notices = poll(self.client, &self.caller.holder);
        loop {
            tokio::select! {
                key = keys.recv(), if typing => match key {
                    Some(CTRL_C) => return self.interrupt().await,
                    Some(CTRL_BRACKET) => {
                        if self.detach().await? {
                            return Ok(());
                        }
                    }
                    Some(_) => {}
                    None => typing = false,
                },
                batch = &mut notices => {
                    notices = poll(self.client, &self.caller.holder);
                    for n in batch? {
                        if self.on_notice(&n, &mut keys).await? {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// Show `n` and act on it; true when the view is done.
    async fn on_notice(
        &self,
        n: &Notice,
        keys: &mut mpsc::UnboundedReceiver<u8>,
    ) -> anyhow::Result<bool> {
        let request = n.request_id.as_ref().map(|r| r.as_str().to_owned());
        let lease = n.lease_id.as_ref().map(|l| l.as_str().to_owned());
        match (self.phase(), n.kind) {
            (Phase::Queued { request: ours }, NoticeKind::Offered)
                if request.as_deref() == Some(ours.as_str()) =>
            {
                let Some(lease) = lease else {
                    self.screen.notice(n);
                    return Ok(false);
                };
                self.set(Phase::Offered {
                    request: ours,
                    lease: lease.clone(),
                });
                self.screen.say(
                    &serde_json::to_value(n)?,
                    &format!(
                        "Offered: {} of {} is yours to take; claiming lease {lease}…",
                        n.unit.as_ref().map_or("a unit", |u| u.as_str()),
                        n.resource.as_ref().map_or("the resource", |r| r.as_str()),
                    ),
                );
                self.claim(&lease, keys).await
            }
            (Phase::Queued { request: ours }, NoticeKind::QueueDropped)
                if request.as_deref() == Some(ours.as_str()) =>
            {
                self.screen.notice(n);
                Err(lease::dropped(n).into())
            }
            (Phase::Held { lease: ours }, NoticeKind::LeaseEnded)
                if lease.as_deref() == Some(ours.as_str()) =>
            {
                self.screen.notice(n);
                Ok(true)
            }
            _ => {
                self.screen.notice(n);
                Ok(false)
            }
        }
    }

    /// Claim the offered `lease`, while Ctrl+C stays live: pressed before
    /// the claim is answered, it withdraws the offer instead.
    async fn claim(
        &self,
        lease: &str,
        keys: &mut mpsc::UnboundedReceiver<u8>,
    ) -> anyhow::Result<bool> {
        let claim = lease::claim(self.client, self.caller, lease);
        tokio::pin!(claim);
        let mut typing = true;
        loop {
            tokio::select! {
                biased;
                key = keys.recv(), if typing => match key {
                    Some(CTRL_C) => {
                        self.interrupt().await?;
                        return Ok(true);
                    }
                    Some(CTRL_BRACKET) => self.say_still_waiting(),
                    Some(_) => {}
                    None => typing = false,
                },
                claimed = &mut claim => {
                    let view = claimed?;
                    self.set(Phase::Held { lease: lease.to_owned() });
                    self.screen.say(&view, &lease::granted_text(&view));
                    self.screen.hint_held();
                    return Ok(false);
                }
            }
        }
    }

    /// Ctrl+C: cancel or release, then exit 130.
    async fn interrupt(&self) -> anyhow::Result<()> {
        self.abandon().await?;
        Err(Exit(EXIT_INTERRUPTED).into())
    }

    /// Leave nothing behind: cancel the request (withdrawing an offer), or
    /// release the lease with its cleanup confirmed.
    async fn abandon(&self) -> anyhow::Result<()> {
        let (value, text) = match self.phase() {
            Phase::Queued { request } => {
                lease::cancel(self.client, self.caller, &request).await?;
                (
                    json!({"cancelled": request}),
                    format!("Cancelled: request {request} is withdrawn; nothing is held."),
                )
            }
            Phase::Offered { request, lease } => {
                match lease::cancel(self.client, self.caller, &request).await {
                    Ok(()) => (
                        json!({"cancelled": request, "offer_withdrawn": lease}),
                        format!(
                            "Cancelled: the offer of lease {lease} is withdrawn unclaimed; \
                             nothing is held."
                        ),
                    ),
                    // The claim got there first: the lease is ours, so
                    // give it back.
                    Err(e) => {
                        tracing::debug!(error = %e, "cancel after the claim went through");
                        lease::finish(self.client, self.caller, &lease).await?;
                        (json!({"released": lease}), released_text(&lease))
                    }
                }
            }
            Phase::Held { lease } => {
                lease::finish(self.client, self.caller, &lease).await?;
                (json!({"released": lease}), released_text(&lease))
            }
        };
        self.screen.say(&value, &text);
        Ok(())
    }

    /// Ctrl+]: keep the lease and leave. A CLI-process holder hands it to a
    /// fresh user holder first, so it outlives this process. True when
    /// detached.
    async fn detach(&self) -> anyhow::Result<bool> {
        let Phase::Held { lease } = self.phase() else {
            self.say_still_waiting();
            return Ok(false);
        };
        if matches!(self.caller.holder, HolderRef::CliProcess { .. }) {
            let holder = HolderRef::User {
                tag: Ulid::new().to_string(),
            };
            self.client
                .call(
                    "rehome_holder",
                    self.caller,
                    json!({"lease_id": lease, "new_holder": holder}),
                )
                .await?;
        }
        self.screen.say(
            &json!({"detached": lease}),
            &format!(
                "Detached: lease {lease} stays held until `clerk release {lease}` or its end; \
                 `clerk attach {lease}` shows it again."
            ),
        );
        Ok(true)
    }

    fn say_still_waiting(&self) {
        self.screen.say(
            &json!({"detached": null}),
            "Nothing to detach yet: the request is still waiting. Ctrl+C cancels it.",
        );
    }
}

fn released_text(lease: &str) -> String {
    format!("Released: lease {lease} has ended and its cleanup is confirmed.")
}
