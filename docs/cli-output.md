# `clerk` human output

The reference for what `clerk` prints on a terminal. Without a terminal, or
with `--json`, every command prints JSON instead and none of this applies.

## Rules

- **IDs are never cut**: resource, unit, lease, request and promotion IDs, and
  settings such as device paths, are printed whole. Only `clerk log` may
  shorten IDs inside its action column; `--wide` or `--json` shows them whole.
- **Free text gets its own line**: descriptions, purposes and audit details sit
  on an indented line (or a wrapped table cell), wrapped to the terminal width.
- **Projects** are shown by the repository folder, with `~` for the home
  directory (`~/dev/leclerk`), never the `.git` directory.
- **Holders**:

  | Holder | Shown as |
  |---|---|
  | your detached CLI lease (`User`) | `cli` |
  | a running CLI process | `pid 130150` |
  | an agent session | `agent 3f2a9c1e` (first 8 characters of its session ID) |
  | a holder in a project you can't see | `agent in another project` |

- **Times** are relative (`in 27m`, `9s ago`); a grant also shows the clock
  time it ends (`until 14:32`). JSON keeps exact values.
- **Multi-line text** (usage) is indented under its label.
- **Empty fields are left out**, e.g. no `unit:` line when the unit has no
  description, no `tags:` line without tags.
- **Style** (terminal only; off with `NO_COLOR` or `--json`):
  - tags: bold cyan;
  - resource IDs: bold;
  - `⚠` and its text: yellow.
  - Nothing else is coloured.
- **Glyphs**: `▮` free unit, `▯` busy unit, `⚠` unhealthy or dirty. When the
  locale isn't UTF-8, or with `--ascii`: `#`, `.` and `!`.
- **Layout**: no tables. Every view is a list of blocks: a header line with
  the IDs and short facts, then indented lines for free text and details.
  - Each top-level block (resource, lease, request, promotion, log entry)
    starts with `•`; nested items (units, queue entries) start with `◦`.
    ASCII fallback: `*` and `-`.
  - A blank line separates blocks of different resources (and every
    top-level block in `resources`, `status`, `leases`, `promotions`). `log`
    keeps one line per entry, without blank lines.
- **Errors**: the first sentence says what happened and names the missing thing
  (`not found: resource "nope"; ...`), the same words for people and agents;
  JSON error messages never carry clap's "try '--help'" tail.

## Mock-ups

`clerk resources`
```
• esp32-c6 ▮▯ 1/2 free · device · user
  ESP32-C6 devkits on the bench hub
  tags: devboard esp32 esp32c6 thread zigbee

• otbr ▯ 0/1 free ⚠ · service · user
  Thread border router REST API
  tags: otbr thread

• nrf52 ▯ 0/1 free · device · project ~/dev/zigbee-fw
  nRF52840 dongles
  tags: nrf52 thread
```

`clerk status`
```
• esp32-c6 ▮▯ 1/2 free · device · user · devboard esp32 esp32c6 thread zigbee
  ESP32-C6 devkits on the bench hub
  ◦ b1 ▯ held by cli · ends in 27m · lse_01M71FRGTW9BC1FSABQRDH7TZ1
      hub port 1 · "third"
  ◦ b2 ▮ free · ⚠ dirty: command_failed 4m ago (pid 129293)
      hub port 2, cracked antenna

• otbr ▯ 0/1 free ⚠ · service · user · otbr thread
  Thread border router REST API
  ◦ main ▯ ⚠ unhealthy: connect failed (checked 12s ago)
  queue:
  ◦ 1. pid 130150 · normal · "check api" · req_01M71FTC912KWSMGKC34CHWA9K
```

`clerk leases`
```
• lse_01M71FRGTW9BC1FSABQRDH7TZ1  esp32-c6/b1 · held · ends in 27m · cli
  "third"

• lse_01M71FSCFXS610PFPXM97CM6FX  nrf52/d1 · held · ends in 3m · agent 3f2a9c1e
  also blocks dongle/x (same device)
```

`clerk queue otbr`
```
• 1. req_01M71FTC912KWSMGKC34CHWA9K · pid 130150 · normal
  "check api"
```

`clerk resource show esp32-c6`
```
esp32-c6  device · user
  ESP32-C6 devkits on the bench hub
  tags:   devboard esp32 esp32c6 thread zigbee
  health: type default (exists, not-open)
  units:
  ◦ b1  /dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_F0:F5:BD:01:02:03-if00
        hub port 1
  ◦ b2  /dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_F0:F5:BD:01:02:04-if00
        hub port 2, cracked antenna
  usage (template):
    Flash: `idf.py -p {unit.path} flash`
    Monitor: `idf.py -p {unit.path} monitor` (Ctrl+] to quit)
```

Grant (`acquire`, `attach`, `run`)
```
Granted: esp32-c6/b2 until 14:32 (in 2h) · lse_01M71FRZ5ATGAMR1GZB0MQF2XZ
  ESP32-C6 devkits on the bench hub
  b2: hub port 2, cracked antenna
  tags: devboard esp32 esp32c6 thread zigbee
  path: /dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_F0:F5:BD:01:02:04-if00
  usage:
    Flash: `idf.py -p /dev/serial/by-id/usb-Espressif_…-if00 flash`
  ⚠ dirty: the last holder's command failed (command_failed); expect a messy state
Ctrl+C releases · Ctrl+] detaches
```
(`…` in this mock-up only shortens the document; real output prints paths whole.)

Queued
```
Queued: position 1 on otbr · req_01M71FTC912KWSMGKC34CHWA9K
  main is free but ⚠ unhealthy (connect failed); it is re-checked every 30s.
Ctrl+C cancels.
```
The second line names what blocks the request: every free unit unhealthy (with
the check's message), units held (holder and purpose of the one ending first),
or a linked unit busy.

`clerk log`
```
• 9s ago   offered      otbr/main → cli · lse_01M71FWE…
• 41s ago  queued       otbr by cli · "late"
• 43s ago  lease_ended  tokens/t1 · released, cleanup confirmed
```

`clerk promotions`
```
• prm_01M71FSCD807F5CX1J7EJ0TBA6  nrf52 · project ~/dev/zigbee-fw
  nRF52840 dongles
  requested by ~/dev/thread-app: a matching unit was registered there
Approve with `clerk approve prm_01M71FSCD807F5CX1J7EJ0TBA6`.
```

## Agent sessions

Run in an agent session (`CLAUDE_CODE_SESSION_ID` set) with stdout not a
terminal, a command prints its result as **labelled Markdown** rendered from
the same JSON scripts get (`src/cli/md.rs`). `--json`, or
`CLERK_AGENT_FORMAT=json`, prints the JSON instead. A terminal keeps the
person view.

- Every non-empty field is a `- key: value` line, in the JSON's (key)
  order; nulls and empty values are left out; nested objects are indented
  lists; lists of scalars are comma-joined; multi-line text (usage) is a
  `### key` section, verbatim.
- Times read `2026-11-10 14:32 (in 20m)`; durations `20m`, labelled without
  `_ms`.
- Grants, queued requests, leases and notices get a heading
  (`## Granted: <resource>/<unit>`, `## Queued: …`, `## Lease <id>`,
  `## Notice: <kind>`) and leave out fields that only restate it or a
  default (the caller's own holder, `held`, `normal` priority, not
  interruptible).
- The summary follows as a paragraph, and the last line is `Next: …`.
- A list is one `## <id>` block per item, without `Next:`.
- An error is `## Error: <code>` and its message (ending with the next
  step), on stdout.

```text
## Granted: c6-boards/c6-a
- duration: 20m
- ends_at: 2026-11-10 14:52 (in 20m)
- lease_id: lse_01M72…
- purpose: flash and test the OTA path
- resource: c6-boards
- resource_description: ESP32-C6 dev boards on the USB hub
- settings:
  - path: /dev/serial/by-id/usb-Espressif_…-if00
  - type: device
- unit: c6-a

### usage
Flash with `idf.py -p /dev/serial/by-id/… flash`. Clean = erase flash.

You hold lease lse_01M72… (c6-boards/c6-a); read its description and usage before using it.

Next: Use the unit, then `clerk release lse_01M72…` when you are done (…).
```
