# Capture & dedup without lifecycle hooks — and what actually has hooks (research)

> Research for RFC #878. Findings that drive the design; see per-app files
> for sources. Rule from RFC #878 that still holds: everything entering the
> store goes through `sanitize()` → `/hook` or `/hook/batch`; never a direct
> wiki/DB write.

## 1. Revised capture map

RFC #878's core claim ("no clean automatic-capture story for closed desktop
apps; Cursor is the exception") needs revision after this research:

| Surface | Capture mechanism | Effort |
| --- | --- | --- |
| Claude Desktop **Code/Cowork** | Already captured: desktop runs Claude Code with user `~/.claude/settings.json` → ai-memory hooks fire (`agent=claude-code`, cwd = scratch workspace). **Zero new code.** | 0 |
| Codex desktop (Codex mode) | Same host config as CLI (`~/.codex/hooks.json`) → hooks expected to fire in-app. **Verify live; then zero code.** | verify |
| Antigravity IDE / 2.0 / CLI | Native hooks (`~/.gemini/config/hooks.json` + `.agents/hooks.json`); payload has `conversationId`, `transcriptPath`, `workspacePaths[]`, `toolCall`. Map to `/hook` observations. ai-memory already ships `install-hooks --agent antigravity-cli`; IDE reads the same global file. | adapter polish |
| Zed (ACP + terminal agents) | Agent's own hooks; Zed transparent. | 0 |
| Claude Desktop **Chat tab**, hosted ChatGPT chat, Grok Bot | No hooks, no local store → model-discretion MCP tool calls (option a) and/or explicit capture tool. | RFC Phase 1 |
| Hosted ChatGPT plugins | Remote-only; needs published plugin + reachable server. | new surface |

### What the MCP server can see server-side (all apps)

ai-memory *is* the MCP server, so on any MCP-connected app we observe:
tool calls (name + args + results), call timestamps, and — through the MCP
session — nothing else. We do **not** see chat turns, prompts, or model
output. Practical consequences:

- Server-side observation is enough for **usage telemetry** (which tools an
  app's model actually calls) and for **recall-effectiveness** signals, not
  for conversation capture.
- It is *also* enough to implement an explicit capture primitive the model
  can be instructed to call (RFC #878 Phase-1 `memory_capture_conversation`
  idea): on apps without any hook surface this is the only in-band path,
  and its quality is bounded by the model's willingness to summarize.

### Local-store watchers (option c) — status

Now concrete per app (paths verified locally unless noted):

- `~/.claude/projects/<munged-cwd>/<session>.jsonl` — Claude Code-family
  transcripts (desktop Code/Cowork included). Watcher optional: hooks
  already cover it; a watcher adds value only for hookless flows (e.g.
  user disabled hooks).
- `~/.codex/sessions/**/rollout-*.jsonl` + `~/.codex/history.jsonl` — Codex.
- `~/.gemini/antigravity{,-ide,-cli}/brain/<conversationId>/**/transcript.jsonl`
  — Antigravity (per hooks doc; not yet verified locally — no IDE chats on
  the machine).
- Zed native agent: internal DB in Zed's data dir — **do not** build on it
  (unstable, version-fragile); rely on ACP agents instead.
- Claude Chat tab / hosted ChatGPT / Grok Bot: **no local store** — watcher
  class does not apply.

A watcher/importer remains the importer-envelope (`/hook/batch`) pattern
from `companions/ai-memory-importer`; keep it out of core (RFC #878 rule).

## 2. Session identity & dedup design

### Claude CLI ↔ Desktop (the case that prompted this)

Verified mechanics (see `claude-desktop.md`):

- Desktop Code sessions **are** Claude Code sessions: one `session_id`
  (CLI UUID). `/desktop` and `claude --desktop --resume <id>` move the same
  id across surfaces. ai-memory's `(agent=claude-code, session_id)` key
  therefore sees **one session**, no dedup needed.
- Desktop Cowork spawns **new** CLI sessions under ephemeral scratch cwds
  (`~/.config/Claude/scratch-workspaces/<acct>/<org>/scratch-YYYY-MM-DD-hex`).
  Two effects to design for:
  1. **Project fragmentation**: every scratch dir becomes its own
     auto-named project. Today that means a stream of one-off projects.
     Options (product decision, not research):
     - (a) detect the scratch-workspace path prefix in scope resolution and
       route to a single `desktop/claude-cowork` project (keeps memory in
       one pool); or
     - (b) leave per-scratch projects but tag them `claude-desktop` via
       session metadata for filtering.
     The desktop-side record (`~/.config/Claude/claude-code-sessions/.../
     local_*.json`) provides `title`, `createdAt`, account/org and the
     `local_* ↔ cliSessionId` mapping if we want to enrich/label — but it is
     an implementation detail of a closed app; treat as optional hint, not
     a dependency (it can move without notice).
  2. **Bridging id exposure**: the desktop id (`local_*`) never reaches
     hooks, so no collision risk with the CLI UUID.
- Live-only correlation: `~/.claude/sessions/<pid>.json` gives
  `entrypoint: claude-desktop` + `hostSessionId` while a process runs. Not
  archival; useful at most for a tray app's "current desktop sessions" view.
- The cc-socks Unix sockets (`/run/user/<uid>/cc-socks/<pid>.sock`) are an
  internal IPC of the closed app — **do not** build on them.

### Codex

One Codex host per user: CLI + desktop + IDE extension share `~/.codex`
(config **and** session rollout store). Session ids are the rollout UUIDs;
`(agent=codex, session_id)` is already correct, and a session started in
the desktop app is distinguishable only by content/absence of TUI markers —
no id-level split. Hosted ChatGPT/Codex-cloud sessions are cloud ids that
never touch the local store; if cloud-side capture ever matters it arrives
via export/API, mapped as a *separate* agent kind (e.g. `codex-cloud`) to
avoid double-counting with local rollouts.

### Antigravity

`conversationId` (UUID) is the session key, identical across IDE/CLI/2.0
for the same conversation; the surface is distinguishable by the
`transcriptPath` prefix (`antigravity-ide/` vs `antigravity-cli/` vs
`antigravity/`). Map to ai-memory: `agent=antigravity`, session_id =
`conversationId`, cwd = `workspacePaths[0]` (fallback: last path; the
payload has no scalar `cwd`). Hook set lacks SessionStart/SessionEnd —
derive session boundaries from first/last observation timestamps per
conversation id (ai-memory sessions are already built from observation
streams, so a Stop-hook flush marker is enough; optional).

### General rule

Dedup key stays `(agent, session_id)`; **surface variants of one agent
ecosystem that share an id space stay one agent kind** (claude-code covers
CLI+desktop; codex covers CLI+desktop+IDE ext; antigravity covers
IDE+CLI+2.0). Where a closed app keeps its own id (desktop `local_*`),
never invent capture from it — treat as metadata only.

## 3. Open questions

1. Do Codex hooks fire for desktop-app sessions? (live test; if no, a
   `~/.codex/sessions` watcher is the fallback for desktop capture.)
2. Does Antigravity IDE honor `~/.gemini/config/hooks.json` in practice, and
   does `workspacePaths` ever contain >1 path on this setup? (live test)
3. Claude Desktop Chat tab: is any local conversation cache planned
   (desktop extensions/plugins evolve fast)? Re-check quarterly; not
   buildable today.
4. Grok Bot `mcpBoxServers` entry format — reverse-engineer only if the
   product gains traction with users; no docs.
5. Scope for scratch-workspace sessions: single `desktop/claude-cowork`
   pool vs per-scratch projects (product decision; affects auto-scope).
