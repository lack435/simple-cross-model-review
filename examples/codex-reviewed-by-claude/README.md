# Codex, reviewed by Claude

The caller is Codex (desktop app or CLI); reviews come back from Claude Opus 4.8.

## Setup

1. Copy `cross-review.exe` into the project and commit it:

   ```
   <your-project>\tools\cross-review.exe
   ```

   It is self-contained — no Node, Python, or DLLs required.

2. Copy [`.codex/config.toml`](.codex/config.toml) to `<your-project>\.codex\config.toml`,
   or merge the `cross_review` entry into the one already there:

   ```toml
   [mcp_servers.cross_review]
   command = "tools/cross-review.exe"
   args = ["--reviewer", "claude", "--model", "claude-opus-4-8", "--effort", "medium"]
   startup_timeout_sec = 30
   tool_timeout_sec = 2400
   ```

   Relative paths resolve against the project root, so this config travels with the repo.

   `tool_timeout_sec` should exceed the server's collect cap (capture budget +
   `--timeout-seconds` + a finalization grace, ~1890s at the defaults), because a single
   `cross_model_review_result` call now blocks until the review is done. Below the cap is no
   longer destructive: abandoning a collect only detaches the wait — the reviewer keeps running
   and the result stays collectible by `review_id` — so a shorter timeout degrades to polling
   rather than discarding a review that was still coming. `cross_model_review_cancel` is what
   stops a reviewer.

3. Reopen the project in Codex.

4. Ask Codex to call `cross_model_review_status`. It should report `ready: yes` along
   with the Claude CLI path and your account.

## The folder must be trusted

Project-level config is only loaded for trusted folders. Codex records trust in your
global `%USERPROFILE%\.codex\config.toml`:

```toml
[projects.'c:\dev\your-project']
trust_level = "trusted"
```

It writes that entry when you approve the folder on first open. If your project config
seems to be ignored, check that this entry exists — an untrusted folder's project config
is not read.

Note also that `codex mcp list` and `codex doctor` report only the **global** config.
Neither will ever list a project-level server, so they cannot be used to verify this
registration. Use `cross_model_review_status` from inside a Codex session instead.

## Requirements

Only on the machine running Codex:

- The Claude Code CLI, installed and signed in (`claude auth login`).

If it is missing or signed out, the tool fails with an explicit message telling the agent
to stop and telling you how to fix it. It never silently falls back to a same-model
review.

## If the Claude CLI is not on PATH

Add `--bin` with its full path:

```toml
args = [
  "--reviewer", "claude",
  "--model", "claude-opus-4-8",
  "--effort", "medium",
  "--bin", "C:\\Users\\you\\.local\\bin\\claude.exe",
]
```

## Registering it for every project instead

If you would rather have it available everywhere, put the same `[mcp_servers.cross_review]`
entry in `%USERPROFILE%\.codex\config.toml`. Keep the `command` relative and it still
resolves per-project, since Codex launches MCP servers with the workspace as their
working directory — at the cost of a failed-server report in repositories that have not
vendored the executable.

## Effort levels

Claude accepts `low`, `medium`, `high`, `xhigh`, and `max`. `medium` is the default here.
Raising it costs more and takes longer.
