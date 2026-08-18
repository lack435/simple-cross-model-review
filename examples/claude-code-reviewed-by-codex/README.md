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

   Keep the `"timeout": 2400000` — milliseconds, and not optional. A single
   `cross_model_review_result` call now blocks until the review is done, up to the server's
   collect cap (capture budget + `--timeout-seconds` + a finalization grace, ~1890s at the
   defaults), so this must exceed that for one blocking call to complete in a single
   round-trip. Below the cap is no longer destructive: abandoning a collect only detaches the
   wait — the reviewer keeps running and the result stays collectible by `review_id` — so a
   shorter timeout degrades to polling rather than discarding a review that was still coming
   (`cross_model_review_cancel` is what stops a reviewer). Setting it here *overrides*
   `MCP_TOOL_TIMEOUT` for this server, making the hard per-call ceiling explicit rather than
   inherited from that variable's ~28-hour default. The 30-minute idle window for a stdio
   server is unaffected: a per-server `timeout` acts as a floor on it, never a cap.

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
  "--level", "standard:gpt-5.6-luna:max",
  "--bin", "C:\\Users\\you\\AppData\\Roaming\\npm\\codex.cmd"
]
```

## Levels

A reviewer's model and effort come from `--level NAME:MODEL:EFFORT`, and at least one is
required. Codex accepts the efforts `low`, `medium`, `high`, `xhigh`, `max`, and `ultra`;
this example pins one `standard` level at `max`. Declare more than one — say a cheaper
`fast` and a deeper `thorough` — and add `--default-level` to pick which an omitted `level`
uses; the caller then selects one per review with the `level` argument. Lowering effort is
cheaper and faster; `ultra`, the only level above `max`, is worth the cost only for
genuinely hard review questions.
