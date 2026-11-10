---
name: clerk
description: Lease shared resources through the clerk, and register new ones. Use before you use a dev board, serial device, port, service, API token or other thing other agents may use at the same time, or that the project's docs or your memory say is leased through the clerk; when a clerk notice (offer, expiry warning, yield request, cleanup window, a brief after /clear or resume) reaches you; and when the user asks to register, add or set up shared things ("register the boards on my hub"), or you need something `clerk resources` does not show. Not for ordinary local work nobody else shares.
---

# The clerk

The clerk hands out shared things (boards, ports, services, tokens) one
holder at a time. You use it through the `clerk` command in your Bash tool.
Every result is Markdown: labelled fields (IDs, times with how far away,
the usage as its own section), what happened, and a last line `Next: …`
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
   over without confirmed cleanup is marked dirty: check it first.
4. **Queued** (exit 6): the result gives your position and request id. The
   wait (below) claims the offer for you when your turn comes.
5. **Keep a wait armed** while you hold or wait for anything (below).
6. **Done:** clean up as the usage says, then `clerk release <lease>`.

## Keep a wait armed

Notices (offers, expiry warnings, yield requests, "your unit is back")
reach you through a wait. Arm exactly one:

- **With the Monitor tool (preferred):** command `clerk wait --follow`,
  `timeout_ms` 1800000, description "clerk notices". Each notice arrives as
  a Monitor event: a `## Notice: <kind>` block, then for an offer the
  `## Lease <id>` you now hold (already claimed for you, with its usage),
  then its own `Next:`. When the monitor expires, arm it again if you still
  hold or wait for anything.
- **Without Monitor:** run `clerk wait` with the Bash tool's
  `run_in_background` (`timeout` 7200000). When it completes, read its
  output file, act on each notice, and start it again.

A second wait exits at once ("already armed"): one is enough. If you end a
turn holding something with no wait armed, a hook reminds you.
`clerk leases --mine` shows what you hold, your queue positions and whether
a wait is armed.

## While you hold it

- **Expiry warning:** `clerk extend <lease> --for 15m` before the end.
  Expiry is hard; an extension may be refused when someone with higher
  priority waits.
- **Yield request** (interruptible leases): pause at a safe point, clean up,
  `clerk confirm <lease> yielded`; or `clerk yield <lease> --refuse`.
- **Overridden:** your lease ends; stop using the unit.

[leasing.md](leasing.md) has the details: cleanup windows, borrowers,
overrides, subagents and `claude -p`.

## Rules

- `--override "<the user's reason>"` only when the user told you to take
  something anyway. It is audited. Never on your own judgement.
- After `/clear` the previous conversation's leases are released; the brief
  at the start says so. Lease again if you still need them.
- If you registered something, record its resource and unit ids in the
  project's docs or your memory so later sessions reuse them.
