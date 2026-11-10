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

## Notices and what to do

Each notice says what happened, which lease or unit, the other party's
purpose, the deadline, and the expected action.

- **offered:** your turn. The wait claims it and the result carries the grant.
- **position changed:** nothing to do.
- **expiry warning:** `clerk extend <lease> --for <d>` or finish up.
- **yield requested:** pause, clean up, `clerk confirm <lease> yielded`, or
  `clerk yield <lease> --refuse`. When the borrower is done you get "your
  unit is back" and your clock resumes. If the borrower overruns you are
  told and may `clerk reclaim <lease>`.
- **cleanup window ending:** confirm cleanup now (below).
- **handed over without confirmed cleanup:** the unit may be dirty; check it.
- **lease ended / overridden:** stop using the unit.

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
- **Headless `claude -p`:** the run ends when you finish. Use the wait only
  while queued, and before you finish release every lease you hold
  (`clerk leases --mine` lists them).

## Example

```text
clerk resources --tag esp32c6
  -> c6-boards (2 units, 1 free)
clerk acquire c6-boards --for 30m --purpose "flash and test the OTA path"
  -> granted c6-a, usage: flash with `idf.py -p {path} flash`; clean = erase
Monitor: clerk wait --follow (timeout_ms 1800000, "clerk notices")
... work; a Monitor event brings an expiry warning ...
clerk extend lse_... --for 15m
... done; erase flash as the usage says ...
clerk release lse_...
```
