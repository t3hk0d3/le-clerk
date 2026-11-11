# Leasing in detail

Read with the clerk skill's SKILL.md. Every command prints Markdown ending
with a `Next:` line; follow it.

## Asking

- `clerk acquire <resource> --for <d> --purpose "<text>"`: any free unit;
  `<resource>/<unit>` for a specific one. `<d>` is `<n>s|m|h|d`.
- `--interruptible`: long work you can pause and resume (soak tests, long
  benchmarks). Others may borrow the unit while you pause; your clock stops
  meanwhile.
- `--priority low|normal|high`: leave it at normal unless the user asked.
- `clerk cancel <request>` drops a queued request you no longer need.
- **Something your own job already uses** (a board your running bench has
  open, a port your server already listens on): the unit's health check
  sees it busy and your request would wait forever. Lease that one unit
  with `clerk acquire <resource>/<unit> --for <d> --skip-checks`: no gap,
  no restart. Never stop the job to lease it (another agent could take the
  unit in between), and never use `--skip-checks` to get past a check that
  says something is wrong. It never takes a unit from a lease holder, and
  it is audited.
- `clerk claim <lease>` claims an offer by hand (the wait normally does).
- Exit 6 means queued, not failed. Asking again for the same target
  returns your existing request instead of a second one; `clerk cancel` it
  first to change it.
- `clerk run <target> --for <d> -- <cmd>` holds a unit while a command
  runs. When the target is queued it exits 6 and the command does not run;
  add `--wait` to wait for the unit and then run it.

## Notices and what to do

Each notice says what happened, which lease or unit, the other party's
purpose, the deadline, and the expected action. The kind is printed after
`## Notice:`.

- `offered`: your turn. The wait claims it and its output carries the lease;
  read its usage with `clerk leases --mine` before you touch the unit.
- `queue_dropped`: your request is gone (your offer lapsed, the resource was
  removed, or the holder went away). Acquire again if you still need it.
- `position_changed`: nothing to do.
- `expiry_warning`: `clerk extend <lease> --for <d>` or finish up.
- `cleanup_window_started`: your lease is ending (expired, released or
  overridden). Clean up as the usage says, then `clerk confirm <lease> clean`.
- `cleanup_ending`: confirm cleanup now, or `clerk extend <lease> --cleanup`
  for the one cleanup extension; otherwise the unit is marked dirty.
- `yield_requested`: pause at a safe point, clean up,
  `clerk confirm <lease> yielded`, or `clerk yield <lease> --refuse`.
- `yield_withdrawn`: carry on; you need not yield.
- `unit_returned`: the borrower is done, your unit is back and your clock
  resumes. If the notice says cleanup was not confirmed, check the unit.
- `borrower_overran`: the borrower is past its time; wait, or
  `clerk reclaim <lease>` after its cleanup window.
- `overridden`: the user took the unit. Stop, clean up and confirm before the
  deadline; acquire again if you still need it.
- `lease_ended`: stop using the unit. If it says the unit was marked dirty,
  you did not confirm cleanup in time.
- `resource_removed`: an admin removed the unit or resource; your lease or
  request is over. Stop using it.

A grant can come marked **dirty**: the previous holder did not confirm
cleanup (or abandoned the unit). Check the unit's state before you use it.

## Handing back

- `clerk release <lease>`: you cleaned up; the lease ends at once.
- `clerk release <lease> --cleanup`: start the cleanup window instead, clean
  up, then `clerk confirm <lease> clean`. An unconfirmed handover marks the
  unit dirty for the next holder. If the work failed and left the unit in an
  unknown state, release with `--cleanup` and do not confirm: the next holder
  is warned.
- `clerk interruptible <lease> on|off` changes interruptibility later.

## Sessions

- Your leases belong to this conversation. `/clear` or quitting releases
  them; `claude --continue` or `--resume` within a minute of a crash keeps
  them.
- **Subagents** share your leases. A subagent should not arm its own wait;
  the main conversation keeps the one wait.
- **Headless `claude -p`:** the run ends when you finish, so nothing can
  wake you later. Before you finish release every lease you hold and cancel
  requests you no longer need (`clerk leases --mine` lists them); the Stop
  hook says the same. Arm a wait only while you keep working.

## Example

```text
clerk resources --tag esp32c6
  -> c6-boards (2 units, 1 free)
clerk acquire c6-boards --for 30m --purpose "flash and test the OTA path"
  -> granted c6-a, usage: flash with `idf.py -p /dev/serial/by-id/usb-Espressif_..._AA-if00 flash`; clean = erase
... work; before a tool call the clerk shows an expiry warning ...
clerk extend lse_... --for 15m
... done; erase flash as the usage says ...
clerk release lse_...
```
