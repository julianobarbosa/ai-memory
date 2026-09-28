# Design proposal: per-project authorization for multi-user servers (#708)

**Status: proposal for review — not implemented.** This is the design pass promised
on #708 before any code lands. It changes a security boundary, so it is deliberately
separated from implementation.

## Problem

On a multi-user server (one where DB users exist, or a trusted identity proxy is
configured — `deployment_distinguishes_operators()` is true), a DB-user token attributes
writes to that user but does **not** scope which projects the user may read or write.
Any authenticated non-root caller can resolve any `(workspace, project)` and read or
write it. Attribution exists; authorization does not. `/admin/*` is already root-only,
and `OwnerFilter` isolates *handoffs* per operator, but pages/observations/search are
project-scoped only, never user-scoped.

This is fine for the default posture (loopback, single operator) and is why it has not
bitten single-user installs. It is a real gap for a shared server hosting several teams.

## Current model (what exists today)

- **Auth ladder** (`ai-memory-core::actor`): `AuthLevel` = `Anonymous` / `User` / `Root`;
  `Capability` = `Admin` / `UserManagement` / `NormalRead` / `NormalWrite` /
  `SkipAdmissionChain`. `authorize(capability, distinguishes_operators)` gates them.
  `NormalRead`/`NormalWrite` are currently granted to everyone (no project dimension).
- **Identity**: `IdentityKey` (storage-key form `user:alice`, `oidc:…`); `ActorContext`
  carries the resolved user. DB users live in the `users` table (token-hash auth).
- **Scope**: `ScopeResolver` resolves `(workspace_id, project_id)`; reads use no-create
  lookups and fail closed on missing scope. `OwnerFilter` (pages shared, batons owned)
  applies to handoffs only — invariant #16 forbids it becoming a page-read filter.

## Proposed model

Introduce an explicit, additive **project grant** keyed by `(user, workspace, project,
level)`. Deny-by-default only when a project has *declared* itself access-controlled,
so existing open deployments do not silently lock out on upgrade.

### Grant levels
`read` (query/read pages/observations/status/briefing in that project) and `write`
(above + write_page/consolidate/handoff/message/delete). `admin` stays global/root as
today (project-admin is out of scope for v1).

### Schema (new migration, next free V on `release/2.3`)
```sql
CREATE TABLE project_grants (
    workspace_id BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id   BLOB NOT NULL REFERENCES projects(id)   ON DELETE CASCADE,
    user_id      BLOB NOT NULL REFERENCES users(id)      ON DELETE CASCADE,
    level        TEXT NOT NULL CHECK (level IN ('read','write')),
    granted_by   BLOB REFERENCES users(id) ON DELETE SET NULL,
    granted_at   INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, project_id, user_id)
) WITHOUT ROWID;
```
A per-project flag decides enforcement: a project row gains
`access_mode TEXT CHECK (access_mode IN ('open','restricted')) DEFAULT 'open'`. `open`
= today's behavior (any authenticated user). `restricted` = only root, the project
creator, and users with a matching grant.

### Enforcement points (fail closed)
A single choke point, `authorize_project(actor, ws, proj, need: read|write)`, consulted by:
- `ScopeResolver` read/write resolution (the one place every MCP/API/web path already
  funnels through), and
- the writer actor as defense in depth for destructive ops.

`open` project → allow (current behavior). `restricted` → allow root; allow a `write`
grant for writes and a `read`/`write` grant for reads; else `AuthzError::Forbidden`.
Anonymous is denied on any restricted project. This must **not** reintroduce the
invariant-#16 hazard: the grant gate is an *authorization* check that returns
allow/deny; it is not an `OwnerFilter` on page rows. A team with grants still sees the
same shared pages — grants gate entry to the project, not row visibility within it.

### Management surface (root-only, `/admin/*`)
`ai-memory user grant --user alice --workspace w --project p --level write` and
`… revoke …`, plus `ai-memory project access --workspace w --project p --mode restricted|open`.
REST under `/admin/projects/*` and `/admin/users/*`, mirroring existing admin routes.

### Migration / rollout
Additive: every existing project defaults to `access_mode='open'`, so nothing changes
for current deployments on upgrade. An operator opts a project into `restricted`
explicitly, then grants users. Single-user/loopback is unaffected (no DB users →
`distinguishes_operators()` false → gate is a no-op).

## Open questions for review
1. **Default for NEW projects on a multi-user server**: keep `open` (least surprise) or
   `restricted` to the creator (secure-by-default)? Proposal: `open`, with a server
   config `[auth] new_projects_restricted = true` to flip it — so secure-by-default is
   available without breaking the common case.
2. **Global scope (`_global`)**: read-open to all authenticated users (it is shared
   preference context), never restricted. Writes stay as today.
3. **Interaction with cross-project messaging (V64)**: a `restricted` recipient inbox
   should require the sender to hold a `write` grant on the recipient project, or the
   message is refused — otherwise grants are bypassable via the mailbox. This ties #708
   to the messaging feature and is why it must be designed, not bolted on.
4. **Handoff `OwnerFilter`**: unchanged; grants and owner-batons are orthogonal.

## Non-goals (v1)
Per-project *admin* delegation, row-level ACLs, time-boxed grants, group/role
abstractions. Those can layer on the `project_grants` table later.

## Verification plan (when approved)
Table-driven authorization tests (root / granted-read / granted-write / no-grant /
anonymous × open/restricted projects), a multi-session integration test proving a
non-granted user is refused a restricted project while a granted teammate is admitted
(the invariant-#16 shape that unit tests miss), and a migration idempotency test.
