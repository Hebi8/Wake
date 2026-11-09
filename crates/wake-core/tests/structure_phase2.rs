use rusqlite::Connection;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use wake_core::adapters::{
    antigravity::AntigravityAdapter, claude::ClaudeAdapter, codex::CodexAdapter, AgentAdapter,
};
use wake_core::db::Store;
use wake_core::models::*;
use wake_core::scanner::{run_scan, NullEvents};

fn write_lines(path: &Path, lines: &[Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
}

fn claude_source(path: &Path) {
    write_lines(
        path,
        &[
            json!({"type":"user","uuid":"alice-user","cwd":"/home/alice/project","message":{"role":"user","content":"Edit and inspect the synthetic files."}}),
            json!({"type":"assistant","uuid":"alice-reply","message":{"id":"alice-api","role":"assistant","usage":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":20,"cache_creation_input_tokens":2},"content":[
                {"type":"tool_use","id":"edit","name":"Edit","input":{"file_path":"src/../edited.rs","old_string":"a","new_string":"b"}},
                {"type":"tool_use","id":"write","name":"Write","input":{"file_path":"new.rs","content":"hello"}},
                {"type":"tool_use","id":"read","name":"Read","input":{"file_path":"../read.rs"}},
                {"type":"tool_use","id":"shell","name":"Bash","input":{"command":"cat untracked.rs > ignored.rs"}}
            ]}}),
        ],
    );
}

fn file_rows(parsed: &ParsedSession) -> Vec<(usize, String, &'static str)> {
    parsed
        .structure
        .iter()
        .flat_map(|m| m.tool_files.iter().map(|f| (f.idx, f.path.clone(), f.op)))
        .collect()
}

#[test]
fn claude_file_tools_are_derived_from_inputs_with_call_indices() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("projects/-home-alice-project/alice.jsonl");
    claude_source(&path);
    let adapter = ClaudeAdapter::new().with_custom_root(dir.path().to_owned());
    let parsed = adapter
        .parse_session(&adapter.file_ref(&path).unwrap())
        .unwrap();
    assert_eq!(
        file_rows(&parsed),
        vec![
            (0, "/home/alice/project/edited.rs".into(), "change"),
            (1, "/home/alice/project/new.rs".into(), "change"),
            (2, "/home/alice/read.rs".into(), "read"),
        ]
    );
}

#[test]
fn codex_patch_paths_survive_the_input_storage_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alice.jsonl");
    let patch = format!("*** Begin Patch\n*** Add File: first.rs\n+{}\n*** Update File: old.rs\n*** Move to: moved.rs\n@@\n-a\n+b\n*** Delete File: removed.rs\n*** Update File: last.rs\n@@\n-a\n+b\n*** End Patch", "x".repeat(18 * 1024));
    write_lines(
        &path,
        &[
            json!({"type":"session_meta","payload":{"id":"alice-codex","cwd":"/home/alice/project"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch","call_id":"patch","input":patch}}),
        ],
    );
    let adapter = CodexAdapter::new().with_custom_root(dir.path().to_owned());
    let r = adapter.file_ref(&path).unwrap();
    let parsed = adapter.parse_session(&r).unwrap();
    let files = file_rows(&parsed);
    assert_eq!(
        files,
        vec![
            (0, "/home/alice/project/first.rs".into(), "change"),
            (0, "/home/alice/project/old.rs".into(), "delete"),
            (0, "/home/alice/project/moved.rs".into(), "change"),
            (0, "/home/alice/project/removed.rs".into(), "delete"),
            (0, "/home/alice/project/last.rs".into(), "change"),
        ]
    );
    let stored = parsed.structure[0].tool_calls[0].input.as_deref().unwrap();
    assert!(!stored.contains("last.rs"));
    assert!(stored.len() < MAX_TOOL_IO + 100);
}

#[test]
fn antigravity_file_tools_use_meta_cwd_and_decode_path_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("antigravity-acp");
    let path = app.join("brain/alice/.system_generated/logs/transcript.jsonl");
    write_lines(
        &path,
        &[
            json!({"type":"PLANNER_RESPONSE","step_index":1,"tool_calls":[
                {"name":"client_view_file","args":{"absolute_path":"\"src/../read.rs\""}},
                {"name":"client_create_file","args":{"target_file":"\"new.rs\""}},
                {"name":"replace_file_content","args":{"TargetFile":"\"/home/alice/edit.rs\""}}
            ]}),
        ],
    );
    fs::create_dir_all(app.join("conversations")).unwrap();
    fs::write(
        app.join("conversations/alice.meta"),
        r#"{"cwd":"/home/alice/project"}"#,
    )
    .unwrap();
    let adapter = AntigravityAdapter::new().with_custom_root(dir.path().to_owned());
    let parsed = adapter
        .parse_session(&adapter.file_ref(&path).unwrap())
        .unwrap();
    assert_eq!(
        file_rows(&parsed),
        vec![
            (0, "/home/alice/project/read.rs".into(), "read"),
            (1, "/home/alice/project/new.rs".into(), "change"),
            (2, "/home/alice/edit.rs".into(), "change"),
        ]
    );
}

#[test]
fn format_one_index_gets_usage_columns_and_files_on_unchanged_source_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("claude");
    claude_source(&root.join("projects/-home-alice-project/alice.jsonl"));
    let adapters = vec![ClaudeAdapter::new().with_custom_root(root)];
    let db = dir.path().join("index.db");
    {
        let store = Arc::new(Store::open(&db).unwrap());
        run_scan(&adapters, &store, &NullEvents, false).unwrap();
    }
    let conn = Connection::open(&db).unwrap();
    let old_stamp: (i64, i64) = conn
        .query_row("SELECT file_mtime,file_size FROM sessions", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    // Reconstruct the actual Phase 1 schema, including its version stamp.
    conn.execute_batch("DROP TABLE tool_files;
        ALTER TABLE message_extras RENAME TO extras_v2;
        CREATE TABLE message_extras(session_key TEXT NOT NULL,seq INTEGER NOT NULL,kind TEXT NOT NULL,thinking TEXT,model TEXT,full_text TEXT,PRIMARY KEY(session_key,seq));
        INSERT INTO message_extras SELECT session_key,seq,kind,thinking,model,full_text FROM extras_v2;
        DROP TABLE extras_v2;
        UPDATE schema_meta SET value='1' WHERE key='structure_format';
        UPDATE structure_sessions SET format='1';").unwrap();
    let store = Arc::new(Store::open(&db).unwrap());
    assert!(store.needs_structure_reindex());
    run_scan(&adapters, &store, &NullEvents, false).unwrap();
    assert!(!store.needs_structure_reindex());
    let new_stamp: (i64, i64) = conn
        .query_row("SELECT file_mtime,file_size FROM sessions", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(new_stamp, old_stamp);
    let usage: (i64, i64, i64, i64) = conn.query_row("SELECT input_tokens,output_tokens,cache_read_tokens,cache_write_tokens FROM message_extras WHERE input_tokens IS NOT NULL", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
    assert_eq!(usage, (10, 3, 20, 2));
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM tool_files", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        conn.query_row("SELECT format FROM structure_sessions", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "2"
    );
}

#[test]
fn codex_code_mode_long_patch_keeps_the_last_file_before_input_clipping() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("alice.jsonl");
    let patch=format!("*** Begin Patch\n*** Add File: first.rs\n+{}\n*** Update File: last.rs\n@@\n-a\n+b\n*** End Patch", "x".repeat(20*1024));
    let code = format!(
        "const patch = {}; text(await tools.apply_patch(patch));",
        serde_json::to_string(&patch).unwrap()
    );
    write_lines(
        &path,
        &[
            json!({"type":"session_meta","payload":{"id":"alice-code-mode","cwd":"/home/alice/project"}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"exec","call_id":"patch","arguments":code}}),
        ],
    );
    let adapter = CodexAdapter::new().with_custom_root(dir.path().to_owned());
    let r = adapter.file_ref(&path).unwrap();
    let parsed = adapter.parse_session(&r).unwrap();
    assert_eq!(
        file_rows(&parsed),
        vec![
            (0, "/home/alice/project/first.rs".into(), "change"),
            (0, "/home/alice/project/last.rs".into(), "change")
        ]
    );
    assert!(!parsed.structure[0].tool_calls[0]
        .input
        .as_deref()
        .unwrap()
        .contains("last.rs"));
}

#[test]
fn stored_usage_matches_session_accounting_after_unindexed_hosts_are_folded() {
    for agent in ["claude", "codex"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(if agent == "claude" {
            "projects/-home-alice-project/alice.jsonl"
        } else {
            "alice.jsonl"
        });
        let rows = if agent == "claude" {
            vec![
                json!({"type":"assistant","message":{"id":"a","role":"assistant","usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":4,"cache_creation_input_tokens":1},"content":[{"type":"tool_use","id":"lookup","name":"Bash","input":{"command":"wake-cli search synthetic"}}]}}),
                json!({"type":"assistant","message":{"id":"b","role":"assistant","usage":{"input_tokens":20,"output_tokens":3,"cache_read_input_tokens":5,"cache_creation_input_tokens":2},"content":[{"type":"text","text":"Visible synthetic answer"}]}}),
            ]
        } else {
            vec![
                json!({"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"Synthetic reasoning"}]}}),
                json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":2,"cached_input_tokens":4},"total_token_usage":{"total_tokens":12}}}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Visible synthetic answer"}]}}),
                json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":20,"output_tokens":3,"cached_input_tokens":5},"total_token_usage":{"total_tokens":35}}}}),
            ]
        };
        write_lines(&path, &rows);
        let adapter: Box<dyn AgentAdapter> = if agent == "claude" {
            ClaudeAdapter::new().with_custom_root(dir.path().to_owned())
        } else {
            CodexAdapter::new().with_custom_root(dir.path().to_owned())
        };
        let r = adapter.file_ref(&path).unwrap();
        let parsed = adapter.parse_session(&r).unwrap();
        assert_eq!(parsed.units.len(), 1);
        let db = dir.path().join("index.db");
        let store = Store::open(&db).unwrap();
        store
            .write_session_guarded(
                &parsed.meta,
                r.mtime_ms,
                &parsed.units,
                &parsed.wake_lookups,
                &parsed.structure,
                &|_| 0,
                None,
            )
            .unwrap();
        let conn = Connection::open(db).unwrap();
        let (input,output,read,write):(i64,i64,i64,i64)=conn.query_row("SELECT SUM(COALESCE(input_tokens,0)),SUM(COALESCE(output_tokens,0)),SUM(COALESCE(cache_read_tokens,0)),SUM(COALESCE(cache_write_tokens,0)) FROM message_extras",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(
            Some(input + output + write + if agent == "codex" { read } else { 0 }),
            parsed.meta.tokens_used
        );
    }
}
