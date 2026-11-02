//! Structure is an optional companion to existing search rows, never their replacement.
use rusqlite::{params, Connection};
use serde_json::json;
use wake_core::adapters::{claude::ClaudeAdapter, AgentAdapter};
use wake_core::db::{Store, FTS_FORMAT, STRUCTURE_FORMAT};
use wake_core::models::*;

fn parse(dir: &std::path::Path, tail: &str, mtime: i64) -> (ParsedSession, ParsedTranscript) {
    let path = dir.join("synthetic.jsonl");
    let children = dir.join("synthetic/subagents");
    std::fs::create_dir_all(&children).unwrap();
    std::fs::write(children.join("agent-child.jsonl"), "{}\n").unwrap();
    std::fs::write(
        children.join("agent-child.meta.json"),
        r#"{"toolUseId":"call"}"#,
    )
    .unwrap();
    let body = format!("{}{}", "a".repeat(32 * 1024), tail);
    let lines = [
        json!({"type":"user","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"ordinary question"}}),
        json!({"type":"assistant","timestamp":"2026-01-01T00:00:01Z","message":{"id":"reply","role":"assistant","model":"test-model","content":[{"type":"text","text":body},{"type":"thinking","thinking":"synthetic reasoning"},{"type":"tool_use","id":"call","name":"Task","input":{"description":"synthetic task"}}]}}),
        json!({"type":"user","timestamp":"2026-01-01T00:00:02Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","is_error":true,"content":"agentId: child-123"}]}}),
        json!({"type":"user","isMeta":true,"message":{"role":"user","content":"synthetic metadata"}}),
        json!({"type":"system","subtype":"compact_boundary"}),
    ];
    let text = lines.iter().map(|v| format!("{v}\n")).collect::<String>();
    std::fs::write(&path, &text).unwrap();
    let reference = SessionFileRef {
        agent: AgentId::ClaudeCode,
        native_id: "synthetic".into(),
        file_path: path.to_string_lossy().into_owned(),
        mtime_ms: mtime,
        size: text.len() as i64,
    };
    let adapter = ClaudeAdapter::new().with_custom_root(dir.to_path_buf());
    (
        adapter.parse_session(&reference).unwrap(),
        adapter.parse_transcript(&reference).unwrap(),
    )
}

fn write(store: &Store, parsed: &ParsedSession, mtime: i64) -> anyhow::Result<bool> {
    store.write_session_guarded(
        &parsed.meta,
        mtime,
        &parsed.units,
        &parsed.wake_lookups,
        &parsed.structure,
        &|_| 0,
        None,
    )
}

fn rows(conn: &Connection) -> Vec<(i64, i64, String)> {
    conn.prepare("SELECT id, seq, text FROM messages ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn long_body_changes_update_structure_without_replacing_search_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    let store = Store::open(&db).unwrap();
    let conn = Connection::open(&db).unwrap();
    let (before, transcript) = parse(dir.path(), "first ending", 1);
    assert_eq!(before.units.len(), 2);
    assert_eq!(
        before.structure.len(),
        2,
        "meta and compaction have no side rows"
    );
    let reply = &transcript.mainline[1];
    assert!(reply.truncated);
    assert!(reply.full_text.as_ref().unwrap().ends_with("first ending"));
    assert!(!serde_json::to_value(reply)
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("full_text"));
    write(&store, &before, 1).unwrap();
    let original = rows(&conn);
    assert!(original[1].2.len() < 33 * 1024);
    assert!(!original[1].2.contains("first ending"));
    let call: (String, String, bool, Option<String>) = conn
        .query_row(
            "SELECT call_id, name, is_error, child_id FROM tool_calls",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        call,
        (
            "call".into(),
            "Task".into(),
            true,
            Some("agent-child".into())
        )
    );
    let extra: (String, String, String, String) = conn
        .query_row(
            "SELECT kind, thinking, model, full_text FROM message_extras",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(extra.0, "text");
    assert_eq!(extra.1, "synthetic reasoning");
    assert_eq!(extra.2, "test-model");
    assert_eq!(extra.3, reply.full_text.as_ref().unwrap().as_str());
    let (after, _) = parse(dir.path(), "second ending", 2);
    write(&store, &after, 2).unwrap();
    assert_eq!(rows(&conn), original);
    let full: String = conn
        .query_row("SELECT full_text FROM message_extras", [], |r| r.get(0))
        .unwrap();
    assert!(full.ends_with("second ending"));
    let stamp: (i64, i64, String, i64, String) = conn
        .query_row(
            "SELECT file_mtime, file_size, fts_format, rows, format FROM structure_sessions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        stamp,
        (
            2,
            after.meta.size_bytes,
            FTS_FORMAT.into(),
            2,
            STRUCTURE_FORMAT.into()
        )
    );
    assert!(store
        .search("second ending", &[], None, 10)
        .unwrap()
        .0
        .is_empty());
    // A stock writer leaves side rows behind: the stamp comparison rejects them.
    conn.execute("UPDATE sessions SET file_mtime = 3", [])
        .unwrap();
    let fresh = || {
        conn.query_row("SELECT COUNT(*) FROM structure_sessions st JOIN sessions s ON s.key = st.session_key WHERE st.file_mtime = s.file_mtime AND st.file_size = s.file_size AND st.fts_format = (SELECT value FROM schema_meta WHERE key = 'fts_format') AND st.rows = (SELECT COUNT(*) FROM messages m WHERE m.session_key = s.key)", [], |r| r.get::<_, i64>(0)).unwrap()
    };
    assert_eq!(fresh(), 0);
    conn.execute("UPDATE sessions SET file_mtime = 2", [])
        .unwrap();
    assert_eq!(fresh(), 1);
    conn.execute(
        "UPDATE schema_meta SET value = 'stock-next' WHERE key = 'fts_format'",
        [],
    )
    .unwrap();
    assert_eq!(fresh(), 0);
    conn.execute(
        "UPDATE schema_meta SET value = ?1 WHERE key = 'fts_format'",
        [FTS_FORMAT],
    )
    .unwrap();
    conn.execute("DELETE FROM messages WHERE seq = 0", [])
        .unwrap();
    assert_eq!(fresh(), 0);
}

#[test]
fn failed_side_write_rolls_back_messages_and_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    let store = Store::open(&db).unwrap();
    let conn = Connection::open(&db).unwrap();
    let (before, _) = parse(dir.path(), "before", 1);
    write(&store, &before, 1).unwrap();
    let original = rows(&conn);
    conn.execute_batch("CREATE TRIGGER fail_extra BEFORE INSERT ON message_extras BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;").unwrap();
    let (mut after, _) = parse(dir.path(), "after", 2);
    after.units[0].text = "changed search text".into();
    assert!(write(&store, &after, 2).is_err());
    assert_eq!(rows(&conn), original);
    let stamp: i64 = conn
        .query_row("SELECT file_mtime FROM structure_sessions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(stamp, 1);
    let full: String = conn
        .query_row("SELECT full_text FROM message_extras", [], |r| r.get(0))
        .unwrap();
    assert!(full.ends_with("before"));
    conn.execute_batch("DROP TRIGGER fail_extra").unwrap();
    store.remove_session(&before.meta.key).unwrap();
    for table in ["tool_calls", "message_extras", "structure_sessions"] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[test]
fn structure_upgrade_is_independent_of_fts_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    {
        let store = Store::open(&db).unwrap();
        assert!(!store.needs_structure_reindex());
    }
    let conn = Connection::open(&db).unwrap();
    conn.execute("DELETE FROM schema_meta WHERE key = 'structure_format'", [])
        .unwrap();
    for _ in 0..2 {
        let store = Store::open(&db).unwrap();
        assert!(store.needs_structure_reindex());
        assert!(!store.needs_fts_reindex());
    }
    let store = Store::open(&db).unwrap();
    store.finish_structure_reindex().unwrap();
    drop(store);
    assert!(!Store::open(&db).unwrap().needs_structure_reindex());
    let version: String = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key = ?1",
            params!["structure_format"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, STRUCTURE_FORMAT);
}
