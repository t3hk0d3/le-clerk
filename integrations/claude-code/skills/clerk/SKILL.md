---
name: clerk
description: Lease shared resources through the clerk, and register new ones. Use before you use a dev board, serial device, API token or other thing other agents may use at the same time, a port or service other agents or sessions also use (not your own dev server), or anything the project's docs or your memory say is leased through the clerk; to check who holds a resource or what is free; to hand back a lease; when the user says to take it anyway; when a clerk notice (offer, expiry warning, yield request, cleanup window, a brief after /clear or resume) reaches you; and when the user asks to register, add or set up shared things ("register the boards on my hub"), or you need something `clerk resources` does not show. Not for ordinary local work nobody else shares. Not for working on the clerk's own source.
---

# The clerk

The clerk hands out shared things (boards, ports, services, tokens) one
holder at a time. You use it through the `clerk` command in your Bash tool.
Every result is Markdown: labelled fields (IDs, times with how far away,
a `usage` field), what happened, and a last line `Next: …`
with the step the clerk expects. Errors start `## Error: <code>`, then a
message ending with the next step, then their details (e.g. who is
waiting). Follow `Next:`. To process a result in a script (`jq`,
`python3`), add `--json`: it prints the same result as JSON.

**Never coordinate resources by messaging other agents.** No "are you done
with the board?" messages, no claiming in chat or files. The clerk is the
only place leases are taken, queued, yielded and returned.

## The lease loop

1. **Find it:** `clerk resources --tag esp32c6` (every tag must match).
   Nothing fits: it is not registered; read [registering.md](registering.md).
2. **Lease it before you touch it:**
   `clerk acquire <resource> --for 30m --purpose "flash and test OTA"`
   (`<resource>/<unit>` for a specific unit). Declare an honest duration and a
   purpose others can read; `--interruptible` for long work you can pause.
3. **Granted** (exit 0): read the grant's descriptions and `usage` before
   using the unit: how to connect, gotchas, what "clean" means. A unit handed
   over without confirmed cleanup is marked dirty (or abandoned by its previous
   owner): check it first.
4. **Queued** (exit 6): not a failure; **never acquire again** (a repeat
   returns the same request). The result gives your position and request id;
   the wait (below) claims the offer for you when your turn comes. An offer
   arrives already claimed: run `clerk leases --mine` and read its usage
   before you touch the unit.
5. **Queued or interruptible:** keep a wait running (below).
6. **Done:** clean up as the usage says, then `clerk release <lease>`.

## When you need a wait

A wait is how the clerk reaches you while you are idle. You need one **only
while you are queued or hold an `--interruptible` lease**. A plain lease needs
none: do not start or restart a wait just because you hold something.

Run `clerk wait` with the Bash tool's `run_in_background` (`timeout` 7200000),
then get on with your work or end your turn. It costs nothing while idle.
When it completes, read its output file: it holds `## Notice: <kind>` blocks
and, for an offer, the lease you now hold (claimed for you). Act on them, and
start it again only if you are still queued or interruptible.

- A second wait exits at once ("already armed"): one is enough.
  `clerk leases --mine` shows what you hold, your queue positions and whether
  a wait is armed.
- **Subagents never arm a wait**; the main conversation keeps the one.
- **Stop a wait** you no longer need by ending its background task
  (TaskStop); it also ends on its own at its timeout.
- A wait that says this process runs another session is the normal end
  after `/clear`: it was the old session's. Do nothing about it.
- An error `session_not_hosted`: **tell the user** (the hooks are not
  reaching the clerk); do not work around it.

## While you hold it

- **Expiry warning:** `clerk extend <lease> --for 15m` before the end.
  An extension may be refused when someone with higher priority waits.
- **Past your end:** with nobody waiting, you keep the lease. Once someone
  queues for the unit you get a cleanup window; if you don't confirm, the
  unit is handed over as abandoned. Extend if you still need it.
- **Yield request** (interruptible leases): pause at a safe point, clean up,
  `clerk confirm <lease> yielded`; or `clerk yield <lease> --refuse`.
- **Overridden:** your lease ends; stop using the unit.

[leasing.md](leasing.md) has the details: cleanup windows, borrowers,
overrides, subagents and `claude -p`.

## Rules

- `--override "<the user's reason>"` only when the user told you to take
  something anyway. It is audited. Never on your own judgement. Stopping
  the daemon (`clerk daemon stop`) needs it too; `daemon restart` does not.
- `not_yours`: the lease or request belongs to another holder; leave it.
- After `/clear` the previous conversation's leases are released; the brief
  at the start says so. Lease again if you still need them.
- If you registered something, record its resource and unit ids in the
  project's docs or your memory so later sessions reuse them.
