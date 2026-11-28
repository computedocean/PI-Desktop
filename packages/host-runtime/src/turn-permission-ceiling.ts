import type { RacpPermissionMode } from "@pi-desktop/shared";

/** A ceiling-bound turn whose ceiling the Host did not accept never runs. */
export const PERMISSION_CEILING_UNAVAILABLE = "PERMISSION_CEILING_UNAVAILABLE";

const CEILINGS: ReadonlySet<string> = new Set<RacpPermissionMode>(["ask", "accept-edits", "auto"]);

export function isPermissionCeiling(value: unknown): value is RacpPermissionMode {
  return typeof value === "string" && CEILINGS.has(value);
}

/**
 * `session.beginTurn` parameters for a turn held to `ceiling` (spec §7.3).
 * Host-core records the ceiling on the durable turn and applies it to every
 * tool decision of that turn.
 */
export function turnCeilingParams(ceiling: RacpPermissionMode | undefined): { permissionCeiling?: RacpPermissionMode } {
  return ceiling ? { permissionCeiling: ceiling } : {};
}

/**
 * Throw unless the Host confirmed the requested ceiling. A Host that ignored
 * it would run the turn under the stored session mode, so the caller ends the
 * turn instead of starting it.
 */
export function assertTurnCeilingRecorded(
  requested: RacpPermissionMode | undefined,
  response: { permissionCeiling?: unknown } | null | undefined,
): void {
  if (!requested || response?.permissionCeiling === requested) return;
  throw Object.assign(new Error("the Host could not enforce this turn's permission ceiling"), {
    errorCode: PERMISSION_CEILING_UNAVAILABLE,
  });
}
