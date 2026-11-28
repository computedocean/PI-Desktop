//! Per-turn permission ceiling (remote-control spec §7.3, LAN WebUI W06).
//!
//! A turn admitted for a principal under the remote ceiling (a paired LAN
//! browser or a Gateway-routed client) records that ceiling on its durable
//! turn row when it begins. Tool authorization resolves the session mode as
//! before and then applies the stricter of that mode and the ceiling of the
//! turn the call belongs to. A ceiling never widens a mode, never rewrites the
//! session setting and does not apply to any other turn.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::db::Database;
use crate::mode_runs::Authorization;
use crate::sessions;

/// Modes a turn ceiling may name, from most to least restrictive.
pub const CEILING_MODES: [&str; 3] = ["ask", "accept-edits", "auto"];

pub fn is_ceiling_mode(mode: &str) -> bool {
    CEILING_MODES.contains(&mode)
}

/// Permissiveness rank. Anything unrecognized ranks as the most restrictive.
fn rank(mode: &str) -> u8 {
    match mode {
        "auto" => 2,
        "accept-edits" => 1,
        _ => 0,
    }
}

/// The stricter of an effective mode and an optional ceiling.
pub fn clamp(effective: String, ceiling: Option<&str>) -> String {
    match ceiling {
        Some(ceiling) if rank(ceiling) < rank(&effective) => ceiling.to_string(),
        _ => effective,
    }
}

/// Parse the optional `permissionCeiling` of `session.beginTurn`. Absent or
/// `null` means the turn has no ceiling; anything else must be a known mode.
pub fn parse_ceiling(value: Option<&Value>) -> std::result::Result<Option<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(mode)) if is_ceiling_mode(mode) => Ok(Some(mode.clone())),
        Some(_) => Err("permissionCeiling must be ask, accept-edits or auto".into()),
    }
}

/// Record the ceiling of a turn that has just begun.
pub fn record(conn: &Connection, turn_id: &str, ceiling: &str) -> Result<()> {
    if !is_ceiling_mode(ceiling) {
        return Err(anyhow!("invalid permission ceiling {ceiling}"));
    }
    let updated = conn
        .prepare_cached(
            "UPDATE turns SET permission_ceiling = ?2 WHERE id = ?1 AND status = 'running'",
        )?
        .execute(params![turn_id, ceiling])?;
    if updated != 1 {
        return Err(anyhow!("turn {turn_id} is not running"));
    }
    Ok(())
}

/// Ceiling governing one tool call. A call that names a turn of this session
/// answers to that turn, so delegated work keeps its originating turn's
/// policy after the turn ends. A call that names no known turn answers to the
/// session's running turn.
pub fn for_call(
    conn: &Connection,
    session_id: &str,
    turn_id: Option<&str>,
) -> Result<Option<String>> {
    if let Some(turn_id) = turn_id.map(str::trim).filter(|id| !id.is_empty()) {
        let named: Option<Option<String>> = conn
            .prepare_cached(
                "SELECT permission_ceiling FROM turns WHERE id = ?1 AND session_id = ?2",
            )?
            .query_row(params![turn_id, session_id], |row| row.get(0))
            .optional()?;
        if let Some(ceiling) = named {
            return Ok(ceiling);
        }
    }
    Ok(conn
        .prepare_cached(
            "SELECT permission_ceiling FROM turns WHERE session_id = ?1 AND status = 'running'",
        )?
        .query_row(params![session_id], |row| row.get::<_, Option<String>>(0))
        .optional()?
        .flatten())
}

/// The session's own mode: its override unless `inherit`, then the global
/// default, then `ask`.
fn session_mode(db: &Database, session_id: &str) -> Result<String> {
    let own = sessions::session_permission_mode(db, session_id)?.filter(|mode| mode != "inherit");
    Ok(own.unwrap_or_else(|| {
        db.get_setting("app")
            .ok()
            .flatten()
            .and_then(|settings| {
                settings
                    .get("defaultPermissionMode")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|mode| sessions::is_valid_permission_mode(mode) && mode != "inherit")
            .unwrap_or_else(|| "ask".to_string())
    }))
}

/// Effective permission mode for one tool call (D115): the session mode, or a
/// subagent's own scope (ADR 0089), capped for unattended staged modes and
/// then by the ceiling of the turn the call belongs to.
pub fn effective_tool_mode(
    db: &Database,
    session_id: &str,
    turn_id: Option<&str>,
    permission_scope: Option<&str>,
    authorization: &Authorization,
) -> Result<String> {
    let mode = match permission_scope {
        Some(scope) if sessions::is_valid_permission_mode(scope) && scope != "inherit" => {
            scope.to_string()
        }
        _ => session_mode(db, session_id)?,
    };
    let mode = authorization.cap_permission_mode(mode);
    Ok(clamp(
        mode,
        for_call(db.conn(), session_id, turn_id)?.as_deref(),
    ))
}

/// Permission mode a new session inherits from `parent_id`. While the parent
/// runs a ceiling-bound turn, the child cannot start broader than that turn.
pub fn inherited_mode(db: &Database, parent_id: &str) -> Result<Option<String>> {
    let Some(stored) = sessions::session_permission_mode(db, parent_id)? else {
        return Ok(None);
    };
    let Some(ceiling) = for_call(db.conn(), parent_id, None)? else {
        return Ok(Some(stored));
    };
    Ok(Some(clamp(session_mode(db, parent_id)?, Some(&ceiling))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode_runs;

    fn session(db: &Database, mode: &str) -> String {
        let created =
            sessions::create_session(db, None, Some("agent".into()), None, None, None).unwrap();
        sessions::configure_session_with_thinking(
            db,
            &created.id,
            "agent",
            None,
            None,
            None,
            Some(mode),
        )
        .unwrap();
        created.id
    }

    fn authorization(db: &Database, session_id: &str) -> Authorization {
        mode_runs::authorization(db, session_id).unwrap().unwrap()
    }

    #[test]
    fn clamp_only_narrows() {
        assert_eq!(clamp("auto".into(), Some("ask")), "ask");
        assert_eq!(clamp("accept-edits".into(), Some("auto")), "accept-edits");
        assert_eq!(clamp("ask".into(), Some("auto")), "ask");
        assert_eq!(clamp("auto".into(), None), "auto");
        // An unknown ceiling is the most restrictive, never a widening.
        assert_eq!(clamp("ask".into(), Some("unknown")), "ask");
    }

    #[test]
    fn parse_ceiling_accepts_only_known_modes() {
        assert_eq!(parse_ceiling(None), Ok(None));
        assert_eq!(parse_ceiling(Some(&Value::Null)), Ok(None));
        assert_eq!(
            parse_ceiling(Some(&Value::from("ask"))),
            Ok(Some("ask".into()))
        );
        assert!(parse_ceiling(Some(&Value::from("inherit"))).is_err());
        assert!(parse_ceiling(Some(&Value::from(1))).is_err());
    }

    #[test]
    fn a_ceiling_governs_its_own_turn_and_outlives_it_only_for_that_turn() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("pi.sqlite")).unwrap();
        let session_id = session(&db, "auto");
        let phone_turn = sessions::begin_turn(&db, &session_id, None, None).unwrap();
        record(db.conn(), &phone_turn, "ask").unwrap();

        let auth = authorization(&db, &session_id);
        let mode = |turn: Option<&str>, scope: Option<&str>| {
            effective_tool_mode(&db, &session_id, turn, scope, &auth).unwrap()
        };
        assert_eq!(mode(Some(&phone_turn), None), "ask");
        // A call without a turn, or with a foreign one, answers to the running turn.
        assert_eq!(mode(None, None), "ask");
        assert_eq!(mode(Some("unknown-turn"), None), "ask");
        // A subagent's own scope cannot lift the turn above its ceiling.
        assert_eq!(mode(Some(&phone_turn), Some("auto")), "ask");
        // Neither can a later session setting change.
        sessions::configure_session_with_thinking(
            &db,
            &session_id,
            "agent",
            None,
            None,
            None,
            Some("auto"),
        )
        .unwrap();
        assert_eq!(mode(Some(&phone_turn), None), "ask");

        sessions::end_turn(&db, &phone_turn, "completed", None, None, false).unwrap();
        // Delegated work of the finished phone turn keeps its ceiling...
        assert_eq!(mode(Some(&phone_turn), None), "ask");
        // ...while the next Desktop turn uses the unchanged session setting.
        let desktop_turn = sessions::begin_turn(&db, &session_id, None, None).unwrap();
        assert_eq!(mode(Some(&desktop_turn), None), "auto");
        assert_eq!(mode(None, None), "auto");
        assert_eq!(
            sessions::session_permission_mode(&db, &session_id)
                .unwrap()
                .as_deref(),
            Some("auto")
        );
    }

    #[test]
    fn an_inherited_session_default_is_resolved_before_the_ceiling_applies() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("pi.sqlite")).unwrap();
        db.set_setting(
            "app",
            &serde_json::json!({ "defaultPermissionMode": "auto" }),
        )
        .unwrap();
        let session_id = session(&db, "inherit");
        let auth = authorization(&db, &session_id);
        assert_eq!(
            effective_tool_mode(&db, &session_id, None, None, &auth).unwrap(),
            "auto"
        );
        let turn = sessions::begin_turn(&db, &session_id, None, None).unwrap();
        record(db.conn(), &turn, "ask").unwrap();
        assert_eq!(
            effective_tool_mode(&db, &session_id, Some(&turn), None, &auth).unwrap(),
            "ask"
        );
        // A child session created during the phone turn starts no broader.
        assert_eq!(
            inherited_mode(&db, &session_id).unwrap().as_deref(),
            Some("ask")
        );
        sessions::end_turn(&db, &turn, "completed", None, None, false).unwrap();
        assert_eq!(
            inherited_mode(&db, &session_id).unwrap().as_deref(),
            Some("inherit")
        );
    }

    #[test]
    fn record_refuses_a_turn_that_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("pi.sqlite")).unwrap();
        let session_id = session(&db, "auto");
        let turn = sessions::begin_turn(&db, &session_id, None, None).unwrap();
        sessions::end_turn(&db, &turn, "completed", None, None, false).unwrap();
        assert!(record(db.conn(), &turn, "ask").is_err());
        assert!(record(db.conn(), "missing", "ask").is_err());
    }
}
