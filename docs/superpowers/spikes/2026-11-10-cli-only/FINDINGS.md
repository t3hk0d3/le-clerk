# CLI-only spike (2026-11-10, Claude Code 2.1.321)

Question: can CLI + skills replace the MCP server? Tested Claude Code's own
mechanisms with a stand-in for `clerk wait` (`waiter.sh`: blocks until a flag
file exists, then prints a notice). Isolated: no plugin, no MCP
(`--strict-mcp-config`), user settings not read, a folder trusted earlier,
private tmux server. Model haiku.

1. **Background Bash wakes an idle session: yes.** The agent started the
   waiter with `run_in_background` and ended its turn; after 2.5 min idle the
   flag was touched. Claude Code started a turn on its own ("Background
   command … completed (exit code 0)"), the agent read the output file and
   followed the notice (`evidence/a-2.txt`). The notification carries only a
   pointer to the output file, not the output: the agent must read it.
2. **A background waiter survives `/clear`: yes; its exit wakes the new
   conversation: yes**, but that conversation has no context, so it did not
   act ("I don't have a request from you yet", `evidence/b-2.txt`).
   Something must tell it (e.g. SessionStart `source: clear` hook context).
3. **Bash sees `CLAUDE_PID`: yes**, the Claude Code process, the same before
   and after `/clear`; `CLAUDE_CODE_SESSION_ID` changes on `/clear`
   (`evidence/waiter.log`).
4. **`kill -9` of Claude Code leaves the background waiter running**
   (orphaned). A waiter must watch `CLAUDE_PID` and exit itself; liveness
   must come from the Claude Code process, not from the waiter.

Not checked: background commands running longer than ~3 min (any limit like
MCP's 30-min idle), `--continue` with a waiter running, desktop app / VS Code.
