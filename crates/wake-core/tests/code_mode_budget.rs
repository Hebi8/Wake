//! A malformed exec must not stop the real indexing process or its healthy peers.
#[path = "fixtures/code_mode_budget/cases.rs"]
mod cases;
use serde_json::json;
use std::process::Command;

#[test]
fn real_cli_indexes_every_adversarial_exec_and_a_healthy_session() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let sessions = home.join(".codex/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    for name in cases::ATTACKS
        .iter()
        .copied()
        .chain(std::iter::once("healthy"))
    {
        let code = if name == "healthy" {
            include_str!("fixtures/code_mode_budget/literals.js").to_owned()
        } else {
            cases::script(name)
        };
        let rows = [
            json!({"type":"session_meta","payload":{"id":format!("alice-{name}"),"cwd":"/home/alice/project"}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"exec","call_id":"patch","arguments":code}}),
        ];
        std::fs::write(
            sessions.join(format!("rollout-alice-{name}.jsonl")),
            rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
    }
    let db = tmp.path().join("wake.db");
    let run = |action: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wake-cli"));
        cmd.env_clear()
            .env("HOME", &home)
            .env("WAKE_HOME", &home)
            .env("USERPROFILE", &home)
            .env("APPDATA", home.join("roaming"))
            .env("LOCALAPPDATA", home.join("local"));
        if let Some(value) = std::env::var_os("SystemRoot") {
            cmd.env("SystemRoot", value);
        }
        cmd.arg("--db").arg(&db).arg(action).output().unwrap()
    };
    let output = run("index");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("UPDATE schema_meta SET value='1' WHERE key='structure_format'; UPDATE structure_sessions SET format='1';").unwrap();
    drop(conn);
    let output = run("refresh");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let conn = rusqlite::Connection::open(&db).unwrap();
    let total = (cases::ATTACKS.len() + 1) as i64;
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM structure_sessions WHERE format='2'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        total
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM structure_failures", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(DISTINCT session_key) FROM tool_files",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        total
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM schema_meta WHERE key='structure_reindex'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}
