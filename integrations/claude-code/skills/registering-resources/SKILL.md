---
name: registering-resources
description: Register shared things with the clerk so agents can lease them. Use when the user asks to register, add or set up boards, devices, ports, services, tokens or other shared resources ("register the boards on my hub", "add the staging API keys to the clerk"), and when you need something to lease that resource_list does not show yet.
---

# Registering resources

A **resource** is a kind of thing agents lease; its **units** are the
interchangeable instances. Finding the things (listing serial ports, reading
configs, asking the user) is your job: the clerk has no scan tool. Once
registered, agents lease them with the `leasing` skill.

## Steps

1. **Search first.** `resource_list` with the tags you expect (`["esp32c6"]`,
   `["postgres"]`). If a resource fits, use it and add units to it instead of
   creating another. `resource_get` shows its units and usage.
2. **Pick the type** (`type_list` shows each type's settings and usage
   placeholders):
   - `device`: reached by a filesystem path (serial boards, USB adapters).
     Prefer stable paths such as `/dev/serial/by-id/...` over `/dev/ttyACM0`.
   - `service`: a host and port. `mode: "connect"` for a server that already
     runs, `mode: "listen"` for a port the holder will bind.
   - `generic`: anything else, named by an `identity_key` (tokens, accounts,
     licence seats, test tenants).
3. **Group interchangeable things** into one resource with one unit each: two
   identical boards are one resource with two units, not two resources. Things
   an agent must tell apart are separate resources.
4. **Fill in the context.** Agents only know what you write here.
   - **ID**: lowercase letters, digits and dashes (`.` and `_` also work),
     starting with a letter or digit, at most 63 characters (`c6-boards`,
     `dev-ports`).
   - **Description**: one line, at most 200 characters, saying what it is; one
     per unit saying which instance it is.
   - **Tags**: what it is, its capabilities and its kind (`esp32`,
     `esp32c6`, `thread`, `zigbee`, `devboard`). Lowercase letters, digits and
     `-`, starting with a letter or digit, at most 32 characters each, at most
     16 per resource. Uppercase is refused.
   - **Usage**: Markdown, at most 4 KiB: how to connect or use it, gotchas, and
     what "clean" means when handing it back. Placeholders are filled in on
     grant. Valid keys depend on the type, and unknown keys are refused:
     - every type: `{resource.id}`, `{resource.description}`, `{unit.id}`,
       `{unit.description}`;
     - `device`: `{unit.path}`;
     - `service`: `{unit.mode}`, `{unit.scheme}`, `{unit.host}`,
       `{unit.port}`, `{unit.service_id}`, `{unit.address}` (`host:port`);
     - `generic`: `{unit.identity_key}`.

     Anything else in braces, like shell `${PORT}` or JSON, passes through
     unchanged.
5. **Create and add units**: `resource_create`, then `unit_add` per unit
   (`settings` carries the type and its fields). Read every result:
   - refused as a duplicate in the same scope (`duplicate_in_scope`): the
     thing is registered; use the resource and unit the error names;
   - refused as `ambiguous_duplicate`: the settings match several units, which
     the error lists. Check which one is this thing and use it, or ask the
     user;
   - warned that it looks like a user-scope unit (`duplicate_of_user_unit`):
     the unit was added and linked to that one, so the two are never leased at
     once. Both are safe to lease; prefer the user-scope one;
   - warned that it looks like another unit of this project
     (`duplicate_of_project_unit`): it was added and linked the same way;
     consider removing the redundant one with `unit_remove`;
   - `similar_in_other_project`: the unit is registered here, and another
     project has a similar one. A promotion to user scope was filed. **Tell the
     user** (the result names the project and the `clerk approve` command), and
     don't add it again or under another ID;
   - any other warning: act on what the result states.
6. **Scope**: leave `scope` out, so the resource lands in this project. Use
   `scope: "user"` only when the user says the thing is shared across their
   projects.
7. **Record the IDs** (resource and units) in the project's docs or your
   memory, so later sessions lease them instead of registering them again.

To fix something later: `resource_update` (description, usage, `tags`, or
`add_tags` / `remove_tags`), `unit_update`, `unit_retire` for a unit that is
going away (removed once its lease ends), `unit_remove` for a free unit.

## Examples

### Device pool: two ESP32-C6 boards

```text
resource_create {"id": "c6-boards", "type": "device",
  "description": "ESP32-C6 dev boards on the USB hub",
  "tags": ["esp32", "esp32c6", "thread", "zigbee", "devboard"],
  "usage": "Flash: `idf.py -p {unit.path} flash monitor`.\nGotcha: hold BOOT while plugging in if the port does not appear.\nClean: `idf.py -p {unit.path} erase-flash`, close the monitor."}
unit_add {"resource": "c6-boards", "id": "c6-a",
  "settings": {"type": "device", "path": "/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_AA:BB-if00"},
  "description": "C6 board, left port, blue sticker"}
unit_add {"resource": "c6-boards", "id": "c6-b",
  "settings": {"type": "device", "path": "/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_CC:DD-if00"},
  "description": "C6 board, right port"}
```

### Listen-port service: ports for dev servers

```text
resource_create {"id": "dev-ports", "type": "service",
  "description": "Local ports for dev servers",
  "tags": ["port", "http", "dev-server"],
  "usage": "Bind your server to {unit.port}, e.g. `PORT={unit.port} npm run dev` or `${PORT}` in scripts; open http://{unit.address}/.\nClean: stop the server so the port is free."}
unit_add {"resource": "dev-ports", "id": "p8080",
  "settings": {"type": "service", "mode": "listen", "host": "127.0.0.1", "port": 8080},
  "description": "port 8080"}
```

### Generic pool: API tokens

The identity key names the token; never put the secret itself in the clerk.

```text
resource_create {"id": "staging-api-tokens", "type": "generic",
  "description": "Staging API tokens, one client at a time each",
  "tags": ["api-token", "staging"],
  "usage": "Read the token: `export API_TOKEN=$(pass show staging/{unit.identity_key})`, then use ${API_TOKEN}.\nGotcha: each token allows 10 requests/s.\nClean: unset API_TOKEN; revoke nothing."}
unit_add {"resource": "staging-api-tokens", "id": "token-1",
  "settings": {"type": "generic", "identity_key": "ci-token-1"},
  "description": "first staging token"}
```
