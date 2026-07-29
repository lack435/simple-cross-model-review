# Claude Code, reviewed by Codex

The caller is Claude Code; reviews come back from OpenAI Codex.

## Setup

1. Copy `cross-review.exe` into the project:

   ```
   <your-project>\tools\cross-review.exe
   ```

   Commit it. Nothing else needs to be installed — the executable is self-contained,
   with no Node, Python, or DLL requirements.

2. Copy [`.mcp.json`](.mcp.json) to the root of your project (or merge the
   `cross-review` entry into an existing `.mcp.json`).

   Keep the `"timeout": 600000` — milliseconds, and not optional. A single
   `cross_model_review_result` poll blocks for up to 300 seconds, and a tool call the
   client gives up on is not free: the server honours `notifications/cancelled` by
   stopping the reviewer, so a client timeout shorter than the poll would discard a
   review that was still coming. Setting it here *overrides* `MCP_TOOL_TIMEOUT` for this
   server, lowering the hard per-call ceiling from that variable's ~28-hour default to
   ten minutes — still double the poll cap, and explicit rather than inherited. The
   30-minute idle window for a stdio server is unaffected: a per-server `timeout` acts as
   a floor on it, never a cap.

3. Restart Claude Code in that directory. It will ask you to approve the new MCP
   server; approve it.

4. Confirm the wiring by asking Claude to call `cross_model_review_status`. It should
   report `ready: yes` along with the Codex CLI path and your ChatGPT login.

## Requirements

Only on the machine running Claude Code:

- The Codex CLI, installed and signed in (`codex login`).

If either is missing, the tool fails with an explicit message telling the agent to stop
and telling you how to fix it. It never silently falls back to a same-model review.

## Notes on the paths

`"command": "tools\\cross-review.exe"` is relative to the directory Claude Code was
started in, which is normally the project root. If you start Claude Code from a
subdirectory, use an absolute path instead:

```json
"command": "C:\\dev\\your-project\\tools\\cross-review.exe"
```

## If the Codex CLI is not on PATH

Add `--bin` with its full path:

```json
"args": [
  "--reviewer", "codex",
  "--model", "gpt-5.6-terra",
  "--effort", "xhigh",
  "--bin", "C:\\Users\\you\\AppData\\Roaming\\npm\\codex.cmd"
]
```

## Effort levels

Codex accepts `low`, `medium`, `high`, `xhigh`, `max`, and `ultra`. `xhigh` is the
default here. Raising it costs more and takes longer; `ultra` and `max` are worth it
only for genuinely hard review questions.
