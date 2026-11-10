# Registering resources

A **resource** is a kind of thing agents lease; its **units** are the
interchangeable instances. Finding the things (listing serial ports, reading
configs, asking the user) is your job: the clerk has no scan command.

## Steps

1. **Search first.** `clerk resources --tag <tag>` with the tags you expect.
   If a resource fits, add units to it instead of creating another.
   `clerk resource show <id>` shows its units and usage.
2. **Pick the type** (`clerk types` lists each type's settings and usage
   placeholders):
   - `device`: reached by a filesystem path (serial boards, USB adapters).
     Prefer stable paths such as `/dev/serial/by-id/...` over `/dev/ttyACM0`.
   - `service`: a host and port; `--listen` for a port the holder will bind,
     otherwise it connects to a running server.
   - `generic`: anything else, named by an identity key (tokens, accounts,
     licence seats, test tenants). Never put a secret itself in the clerk.
3. **Group interchangeable things** into one resource with one unit each:
   two identical boards are one resource with two units.
4. **Fill in the context.** Agents only know what you write here.
   - **ID**: lowercase letters, digits and dashes, at most 63 characters.
   - **Description**: one line per resource and per unit.
   - **Tags**: what it is, its capabilities, its kind (`esp32`, `esp32c6`,
     `thread`, `devboard`); lowercase, at most 16.
   - **Usage**: Markdown, at most 4 KiB: how to use it, gotchas, and what
     "clean" means. Placeholders filled in on grant: `{resource.id}`,
     `{unit.id}`, `{unit.description}`; `device`: `{unit.path}`; `service`:
     `{unit.host}`, `{unit.port}`, `{unit.address}`, `{unit.scheme}`;
     `generic`: `{unit.identity_key}`. Other braces (`${PORT}`) pass through.
5. **Create and add units**, reading every result:
   `clerk resource create <id> --type <type> --description "..." --tag <t>
   --usage-file usage.md`, then per unit `clerk unit add <id> <unit>
   --path ... | --host ... --port ... [--listen] | --key ...
   --description "..."`. Multi-line fields can come from YAML with
   `-f file.yaml` (or `-f -` for stdin).
   - **duplicate in scope:** it is registered; use the resource and unit
     the error names.
   - **ambiguous duplicate:** the settings match several units; check which
     one this is, or ask the user.
   - **duplicate of a user or project unit:** added and linked, so the two
     are never leased at once; prefer the user-scope one.
   - **similar in another project:** a promotion to user scope was filed.
     **Tell the user** (the result names the `clerk approve` command); do not
     register it again under another id.
6. **Scope:** leave `--scope` out, so it lands in this project. Use
   `--scope user` only when the user says it is shared across projects.
7. **Record the ids** in the project's docs or your memory.

Later: `clerk resource update` (`--add-tag`, `--remove-tag`, `--usage`),
`clerk unit update`, `clerk unit retire` for a unit that is going away,
`clerk unit remove` for a free unit.

## Example: two ESP32-C6 boards

```text
clerk resource create c6-boards --type device \
  --description "ESP32-C6 dev boards on the USB hub" \
  --tag esp32 --tag esp32c6 --tag thread --tag devboard \
  --usage "Flash: \`idf.py -p {unit.path} flash monitor\`. Clean: erase-flash, close the monitor."
clerk unit add c6-boards c6-a --path /dev/serial/by-id/usb-Espressif_..._AA-if00 \
  --description "left port, blue sticker"
clerk unit add c6-boards c6-b --path /dev/serial/by-id/usb-Espressif_..._CC-if00 \
  --description "right port"
```
