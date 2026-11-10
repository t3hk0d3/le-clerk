---
name: leasing
description: Lease shared resources through the clerk before using them. Use when you need a dev board, serial device, port, service, API token or other thing that other agents may use at the same time, or that the project's docs or your memory say is leased through the clerk; before flashing, opening, binding or spending it; and whenever a clerk notice (offer, expiry warning, yield request, cleanup window) reaches you. Not for ordinary local work nobody else shares.
---

# Leasing shared resources

The clerk hands out shared things (boards, ports, services, tokens) one holder
at a time. Its tools tell you the next step in every result; follow them. This
skill is the etiquette around them.

**Never coordinate resources by messaging other agents.** No "are you done
with the board?" messages, no claiming in chat or files. The clerk is the only
place leases are taken, queued, yielded and returned.

## 1. Find it and lease it

1. Find the resource: `resource_list` with `tags` (e.g. `["esp32c6"]`); every
   tag must match. If nothing fits, it is not registered: follow the
   `registering-resources` skill.
2. Call `lease_acquire` before you touch the thing:
   - `target`: `{"resource": "<id>"}` for any free unit, or add `"unit"` for a
     specific one.
   - `for`: an honest estimate of how long you need it (`"20m"`, `"2h"`).
     Others in the queue plan around it. Don't pad it; extend later instead.
   - `purpose`: one line that tells waiting agents and the user what you are
     doing ("flash and soak-test the Thread firmware").
   - `interruptible: true` for long work you can pause at a safe point and
     resume (soak tests, long benchmarks). Others may then borrow the unit
     while you pause, and your clock stops meanwhile.
   - `priority`: leave it at the default unless the user asked otherwise.
3. Granted: **read the grant before using the unit.** It carries the resource
   and unit descriptions, the tags and the rendered usage: how to connect, the
   gotchas, what "clean" means. If it says the unit was handed over without
   confirmed cleanup, expect a messy state and check it first.
4. Queued: the result gives your position and a `request_id`. Call
   `lease_wait`; it claims the offer for you when your turn comes.
   `lease_cancel` with that `request_id` drops a request you no longer need.

Every lease call after the grant (`lease_extend`, `lease_release`,
`lease_confirm`, ...) takes the grant's `lease_id`.

## 2. Keep `lease_wait` armed

While you hold a lease or wait in a queue, keep one `lease_wait` running.
**`lease_wait` can block** (up to 20 minutes) until a notice arrives, so when
you call it matters:

- **Interactive session:** call it right after a grant or a queued result. It
  runs as a background task, so you keep working, and its result reaches you
  when a notice arrives. Call it again after each result.
- **Where it blocks** (subagents, headless `claude -p`): do the work first.
  Call `lease_wait` when you are about to idle or end your turn while you still
  hold or wait for something. If you are queued with nothing else to do, call
  it at once: waiting is the work. Mid-turn notices reach you through context
  added before your tool calls.

Either way:

- It returns a batch of notices: offers (already claimed for you, with the
  grant), expiry warnings, yield requests, cleanup reminders, "your unit is
  back". Handle each, then call `lease_wait` again with `ack` set to that
  result's notice ids. Unacknowledged notices come back on the next call.
- An empty batch after a long quiet wait is normal: call it again.
- Notices may also appear in your context before a tool call. Act on them;
  they stay pending until a `lease_wait` acknowledges them.
- If you end a turn holding or queued without a wait running, a Stop hook
  reminds you. Arm `lease_wait` before then.
- `lease_status` shows what you hold, your queue positions and pending
  notices.

## 3. While you hold it

- **Expiry warning:** call `lease_extend` with a new honest `for` before the
  end. Expiry is hard: without an extension the lease ends on time even if
  nobody waits. An extension can be refused when someone with higher priority
  waits; then finish up, or make the lease interruptible with
  `lease_set_interruptible`.
- **Yield request** (interruptible leases): pick a safe point, pause your work,
  clean up, then `lease_confirm` with `how: "yielded"`. If you cannot pause
  safely, `lease_yield_refuse`. When the borrower is done you get "your unit is
  back" and your clock resumes. If the borrower overruns, you are told and may
  `lease_reclaim`.
- **Overridden:** your lease ends. Stop using the unit, clean up and confirm,
  then `lease_acquire` again if you still need it.

## 4. Hand it back

1. Done: `lease_release`. The cleanup window starts.
2. Leave the unit as the usage says "clean" (stop servers, close ports, free
   the device), then `lease_confirm` (`how` defaults to `"clean"`). An
   unconfirmed handover marks the unit dirty for the next holder.
3. The work failed and left the unit in an unknown state: `lease_release` with
   `failed: true` ends it at once and marks it dirty.

## Override: only on the user's word

`override_reason` on `lease_acquire` takes a unit even from a holder that
cannot be interrupted. Use it **only when the user told you to take the
resource anyway**, and put the user's reason in it ("user: demo in 10 minutes,
take the C6 board now"). It is audited. Never override on your own judgement
or because a wait is long.

## Remember what you created

If you registered a resource or unit, record its IDs in the project's docs or
your memory so later sessions lease the same thing instead of registering it
again.

## Example

```text
resource_list {"tags": ["esp32c6"]}
  -> c6-boards (2 units, 1 free)
lease_acquire {"target": {"resource": "c6-boards"}, "for": "30m",
               "purpose": "flash and test the OTA update path"}
  -> granted unit c6-a, /dev/serial/by-id/usb-Espressif_..., usage: flash with
     `idf.py -p /dev/serial/by-id/... flash`; clean = erase flash
lease_wait {}                      # interactive: runs as a background task
... work; lease_wait returns an expiry warning notice n1 ...
lease_extend {"lease_id": "<id>", "for": "15m"}
lease_wait {"ack": ["n1"]}         # re-arm, acknowledging n1
... done ...
lease_release {"lease_id": "<id>"}; erase flash as the usage says
lease_confirm {"lease_id": "<id>"}
```
