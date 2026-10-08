# Desktop support — phased plan (research output, reconciled with RFC #878)

> This file reconciles RFC #878 (github.com/akitaonrails/ai-memory#878) with
> the findings in this directory (2026-10-07). Nothing here is scheduled
> work; each phase lists what would need to change and where the risk sits.

## What the research changed vs the RFC

RFC #878's headline — "connection is solved, capture is the crux" — still
holds, but two of its premises moved:

1. **"Most desktop chat apps expose no hook/event surface"** is now false
   for the *coding* desktop surfaces: Claude Desktop's Code/Cowork tabs are
   Claude Code with the user's hooks (captured today, zero code), Codex
   desktop shares `~/.codex` hooks config (verify live), and **Antigravity
   (IDE included) has first-class lifecycle hooks** with transcript paths
   in the payload. The truly hookless set is now: Claude Desktop Chat tab,
   hosted ChatGPT chat, Grok Bot.
2. **"Claude Desktop .mcpb bundle"** as the Phase-1 one-click route: the
   current official extension surface is **plugins** (marketplace; can
   bundle MCP servers + hooks; the desktop app has a plugin browser).
   `.mcpb` status is ambiguous in current docs — re-validate before
   building anything.

## Phase 0 — docs + verify the free wins (ship anytime)

1. Update `docs/mcp-install.md`:
   - Claude Desktop **Linux** is an official beta; document
     `~/.config/Claude/claude_desktop_config.json` (replace the "not
     officially distributed" note).
   - Add a "desktop apps" section pointing at these research notes.
2. Live verifications (machine with the apps installed):
   - [ ] Codex desktop: confirm `~/.codex/hooks.json` fires for
     desktop-app sessions (watch ai-memory observations + rollouts).
   - [ ] Antigravity IDE: confirm global `~/.gemini/config/hooks.json` is
     honored by the IDE; record `workspacePaths` shape and
     `transcriptPath` layout under `~/.gemini/antigravity-ide/brain/`.
3. Document that Claude Desktop Code/Cowork sessions already flow into
   ai-memory, including the scratch-workspace project naming behavior
   (user-facing note: expect `...-Claude-scratch-workspaces-...` projects).

## Phase 1 — Antigravity IDE capture parity (smallest new-code step)

The research's best new target — not the RFC's `.mcpb`:

- Ensure `install-hooks --agent antigravity-cli` output also satisfies the
  IDE (same global file; verify; add `.agents/hooks.json` workspace install
  option).
- Map hook payloads → `/hook` observations: agent kind `antigravity`
  (covers CLI+IDE+2.0), session = `conversationId`, cwd =
  `workspacePaths[0]`, tool events from PreToolUse/PostToolUse; session
  boundaries from PostInvocation/Stop flushes (no SessionStart/End events).
- Tests: payload-mapping unit tests against the documented schema
  (camelCase fields; `<app_data_dir>` variants per surface).

## Phase 2 — scope & dedup polish for desktop-originated sessions

- Decide scratch-workspace routing: single `desktop/claude-cowork` project
  vs per-scratch projects with a `claude-desktop` tag (see
  `capture-and-dedup.md` §2). This is the only dedup work the research
  found necessary — CLI↔Desktop handoffs keep one session id, so no
  id-level dedup layer is needed.
- Optional metadata enrichment from
  `~/.config/Claude/claude-code-sessions/**/local_*.json` (titles,
  account/org) — strictly optional, closed-app internals.

## Phase 3 — explicit capture for hookless surfaces (RFC Phase 1, adjusted)

- Tool-surface change (23-tool count, MEMORY_INSTRUCTIONS/SNIPPET_BODY,
  prompt-surface regression tests, adversarial boundary test) for an
  explicit capture tool the model calls on apps without hooks (Claude Chat
  tab, Grok Bot, hosted ChatGPT chat). Scoping per RFC: fixed
  `personal/desktop-chat` (optionally per-app) since no cwd exists.
- Grok Bot MCP entry format: only worth reverse-engineering on demand.

## Phase 4 — the non-dev tray installer (RFC Phase 2, refined)

- Tauri v2 vs egui decision, packaging matrix, and the coexistence rules in
  `installer-and-coexistence.md` (detect-before-spawn server governance,
  `server_profiles` inheritance, native-runner path stability, no PATH
  stomping).
- Signing/notarization costs as in RFC #878 (unchanged).

## Explicitly deferred / rejected

- Hosted-ChatGPT plugin (remote MCP to a user-run server): real but a new
  distribution surface; needs published-plugin review requirements
  (<https://developers.openai.com/plugins/deploy/app-review>) — separate
  RFC if demand appears.
- Watching Zed's native-agent database: fragile, version-locked; ACP
  agents already give us capture for free.
- Building on `~/.claude/sessions/<pid>.json` cc-socks IPC or Grok Bot's
  local-exec daemon: closed-app internals, unstable by definition.
- A session-id mapping/alias table keyed on desktop `local_*` ids: not
  needed given one-id sessions; revisit only if Anthropic splits ids.

## Open questions (research-level, blocking nothing)

1. Codex-in-app hook firing (Phase 0 verification).
2. Antigravity IDE honoring global hooks (Phase 0 verification).
3. `.mcpb` vs plugins for Claude Desktop one-click (re-check docs).
4. Scratch-workspace scoping product decision (Phase 2).
5. Tray stack choice + Flatpak feasibility (Phase 4).
6. Does the Claude Desktop Chat tab ever gain a local store/export? (No
   current path; re-check quarterly.)
