# ADR: Host-core enforces the remote permission ceiling per turn

- Status: Accepted
- Date: 2026-11-28
- Related: [Remote-control security §4.3](../spec/05-security/02-remote-control-security.md#43-remote-permission-ceiling), [LAN WebUI MVP](../spec/01-product/03-lan-webui-mvp.md), [LAN WebUI pairing](lan-webui-code-pairing.md)

## Context

The Agent Host computed the remote `ask` ceiling for a paired LAN browser and
other non-exempt principals, but only as admission metadata. The runtime
accepted a per-turn `permissionMode` without using it, and host-core
authorized tools from the session's stored mode. The bridge forwarded the
value only when it differed from the summarized session mode, so a session
using `inherit` (summarized as `ask`) with an `auto` default sent nothing at
all. A phone turn in an `auto` session therefore ran tools without Desktop
approval. Delegates that outlive their turn also identified themselves by the
runtime's current turn, not the turn that started them.

## Decision

The ceiling is a property of the durable turn, owned by host-core.

- The Agent Host attaches `permissionCeiling` to every turn admitted for a
  principal bound by the ceiling, derived from the authenticated principal and
  Host policy. Exempt principals, including the local Desktop, attach none.
- `session.beginTurn` records it in `turns.permission_ceiling`; queued turns
  carry it in `turn_queue.permission_ceiling` (schema v35). Every tool
  authorization applies the stricter of the session mode current at that
  decision and the ceiling of the turn the call names, or of the running turn
  when it names none. Subagent permission scopes, session creation from that
  turn and session messages sent during it are bounded the same way; it cannot
  register an automatic background-job continuation, which would start a later
  Desktop turn.
- A delegate keeps the turn that started it on its Host calls.
- A Host that does not confirm the ceiling gets no turn; the WebUI reports
  `PERMISSION_CEILING_UNAVAILABLE`. The runtime keeps no copy of the ceiling.

## Consequences

Remote turns ask Desktop for approval wherever an `ask` session would, without
changing the session setting or other turns. Queue entries persisted before
v35 use their admitted mode as their ceiling, so a restored remote entry cannot
start broader than it was admitted; a restored Desktop entry may prompt where
the session has since been raised. Older apps refuse the v35 database, as for
every schema step; rollback uses the v34 backup.

## Alternatives

- Keep the ceiling in Host memory: lost on host-core restart while the sidecar
  can still issue calls, so it would fail open.
- Rewrite the session's stored mode for the turn: changes Desktop's setting
  and leaks into concurrent and later Desktop turns.
