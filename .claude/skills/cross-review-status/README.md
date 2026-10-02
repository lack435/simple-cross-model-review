# cross-review-status

A Claude Code mod that draws a band above the prompt showing the cross-review reviews and consults
the session has started, one row each, and where each one stands:

```
cross-review  ⟳ fix-autocrlf               t2  running 4m12s                        ✕
              ✎ feat-x                     t1  changes requested · 3 open
              ✓ docs                       t3  converged
              ✗ other                          RATE_LIMITED
```

Running jobs come first with their elapsed time (redrawn every 5s while one runs), then the three
most recently finished. The `✕` at the right drops the finished rows. The band sits above whatever
else is drawn there (another mod's band, the engine's own) rather than replacing it.

| Glyph | Meaning |
| --- | --- |
| `⟳` cyan | running |
| `✓` green | `converged`, or a consult that was answered |
| `✎` yellow | `changes_requested`, with the open-finding count |
| `⚠` red | `escalate` or `rebaseline` |
| `✗` red | the job failed, with its code (`RATE_LIMITED`, `SESSION_BUSY`, ...) |
| `⊘` gray | cancelled |

It is not part of the `cross-review` binary and the server knows nothing about it. It only watches
the calls the agent makes to the `cross_model_*` tools and reads their responses: the `review_id:`
and `session:` lines of a start, and the machine envelope of a result (`structuredContent`, or the
`CROSS_REVIEW_ENVELOPE_OUT` block in the text). It never changes a call or its result. Any MCP
server name works; it matches on the tool name's `cross_model_*` suffix.

## Using it

In this repository it loads by itself: Claude Code loads a plugin from a project's
`.claude/skills/<name>/` folder. To use it in another repository, copy this folder into that
repository's `.claude/skills/`, or into `~/.claude/skills/` to have it everywhere. Nothing is
built or installed.

The mod API is early access and may change between Claude Code releases. If the band stops
appearing after an update, run `claude plugin validate .claude/skills/cross-review-status`.

## Developing it

```
claude plugin validate .claude/skills/cross-review-status
claude plugin test .claude/skills/cross-review-status
```

The tests feed the hooks canned server responses through the engine; they call no model. Claude
Code writes the API's type declarations into `.claude-plugin/types/` when it loads the mod
(gitignored), which `tsconfig.json` extends for an editor.
