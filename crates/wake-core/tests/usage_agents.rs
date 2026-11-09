//! Synthetic source contracts for optional usage and structured tool failures.
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;
use wake_core::adapters::{
    antigravity::AntigravityAdapter, dsh::DshAdapter, grok::GrokAdapter, hermes::HermesAdapter,
    kimi::KimiAdapter, pi::PiAdapter, AgentAdapter,
};
use wake_core::models::{MessageKind, ParsedTranscript, Role, TokenUsage};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/usage_agents")
        .join(name)
}

fn stage(root: &Path, target: &str, source: &str) -> PathBuf {
    let path = root.join(target);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::copy(fixture(source), &path).unwrap();
    path
}

fn parse(adapter: &dyn AgentAdapter, path: &Path) -> ParsedTranscript {
    let file = adapter.file_ref(path).expect("source reference");
    let transcript = adapter.parse_transcript(&file).unwrap();
    let session = adapter.parse_session(&file).unwrap();
    assert_eq!(transcript.meta.tokens_used, session.meta.tokens_used);
    assert_eq!(transcript.unknown_line_count, 0);
    transcript
}

fn usage(
    input: Option<i64>,
    output: Option<i64>,
    read: Option<i64>,
    write: Option<i64>,
) -> Option<TokenUsage> {
    Some(TokenUsage {
        input,
        output,
        cache_read: read,
        cache_write: write,
    })
}

#[test]
fn antigravity_acp_discovery_cwd_and_planner_usage_exclude_checkpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let path = stage(
        tmp.path(),
        "antigravity-acp/brain/alice-session/.system_generated/logs/transcript.jsonl",
        "antigravity.jsonl",
    );
    stage(
        tmp.path(),
        "antigravity-acp/conversations/alice-session.meta",
        "antigravity.meta",
    );
    let adapter = AntigravityAdapter::new().with_custom_root(tmp.path().to_path_buf());
    let refs = adapter.list_session_files().unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].native_id, "alice-session");
    let t = parse(adapter.as_ref(), &path);
    assert_eq!(t.meta.project_path, "/home/alice/project");
    assert_eq!(t.meta.tokens_used, Some(56));
    assert_eq!(
        t.mainline[1].usage,
        usage(Some(12), Some(4), Some(40), None)
    );
    assert_eq!(t.mainline[2].kind, MessageKind::CompactSummary);
    assert_eq!(t.mainline[2].usage, None);
    assert_eq!(t.mainline[3].usage, usage(None, Some(0), None, None));
    assert!(t.mainline[3].text.is_empty());
    assert_eq!(t.mainline.last().unwrap().usage, None);
}

#[test]
fn antigravity_app_root_and_meta_changes_are_discoverable() {
    let tmp = tempfile::tempdir().unwrap();
    let path = stage(
        tmp.path(),
        "antigravity-acp/brain/alice-session/.system_generated/logs/transcript.jsonl",
        "antigravity.jsonl",
    );
    let meta = stage(
        tmp.path(),
        "antigravity-acp/conversations/alice-session.meta",
        "antigravity.meta",
    );
    let adapter = AntigravityAdapter::new().with_custom_root(tmp.path().join("antigravity-acp"));
    let before = adapter.list_session_files().unwrap().remove(0);
    fs::write(meta, r#"{"cwd":"/home/alice/second-project"}"#).unwrap();
    let after = adapter.file_ref(&path).unwrap();
    assert_ne!(before.size, after.size);
    assert_eq!(
        parse(adapter.as_ref(), &path).meta.project_path,
        "/home/alice/second-project"
    );
}

#[test]
fn dsh_merges_distinct_calls_and_keeps_usage_only_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let path = stage(tmp.path(), "session.v4.jsonl", "dsh.jsonl");
    let adapter = DshAdapter::new().with_custom_root(tmp.path().to_path_buf());
    let t = parse(adapter.as_ref(), &path);
    assert_eq!(t.meta.tokens_used, Some(51));
    assert_eq!(
        t.mainline[0].usage,
        usage(Some(14), Some(2), Some(30), Some(5))
    );
    assert_eq!(t.mainline[2].usage, usage(None, Some(0), None, None));
    assert!(t.mainline[2].text.is_empty());
}

#[test]
fn dsh_inherited_usage_is_excluded_even_without_a_new_user_row() {
    let tmp = tempfile::tempdir().unwrap();
    let path = stage(tmp.path(), "session.v4.jsonl", "dsh-fork.jsonl");
    let adapter = DshAdapter::new().with_custom_root(tmp.path().to_path_buf());
    let t = parse(adapter.as_ref(), &path);
    assert_eq!(t.meta.tokens_used, Some(6));
    assert_eq!(t.mainline.len(), 2);
    assert_eq!(t.mainline[0].kind, MessageKind::Meta);
    assert_eq!(t.mainline[0].usage, None);
    assert_eq!(t.mainline[1].usage, usage(Some(4), Some(2), None, None));
}

#[test]
fn pi_preserves_cache_split_and_usage_only_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let path = stage(tmp.path(), "alice-pi.jsonl", "pi.jsonl");
    let adapter = PiAdapter::new().with_custom_root(tmp.path().to_path_buf());
    let t = parse(adapter.as_ref(), &path);
    assert_eq!(t.meta.tokens_used, Some(52));
    assert_eq!(
        t.mainline[0].usage,
        usage(Some(14), Some(3), Some(30), Some(5))
    );
    assert_eq!(t.mainline[0].text, "Checked.");
    assert_eq!(t.mainline[2].usage, usage(None, Some(0), None, None));
    assert!(t.mainline[2].text.is_empty());
    assert_eq!(t.mainline[4].usage, None);
}

#[test]
fn kimi_usage_follows_steps_and_preserves_tool_and_fork_mapping() {
    let tmp = tempfile::tempdir().unwrap();
    let path = stage(
        tmp.path(),
        "sessions/wd_alice_1/session_alice/agents/main/wire.jsonl",
        "kimi.jsonl",
    );
    let adapter = KimiAdapter::new().with_custom_root(tmp.path().to_path_buf());
    let t = parse(adapter.as_ref(), &path);
    assert_eq!(t.meta.tokens_used, Some(31));
    assert_eq!(t.mainline[0].kind, MessageKind::Meta);
    assert_eq!(t.mainline[0].usage, None);
    assert_eq!(t.mainline[1].usage, usage(Some(14), Some(3), None, None));
    assert_eq!(
        t.mainline[1].tool_calls[0].output.as_deref(),
        Some("Synthetic contents.")
    );
    assert_eq!(t.mainline[2].usage, usage(None, Some(0), None, None));
    assert_eq!(
        t.mainline[3].usage,
        usage(Some(3), Some(1), Some(8), Some(2))
    );
}

#[test]
fn grok_completion_is_a_prompt_snapshot_not_an_api_call_or_session_increment() {
    let tmp = tempfile::tempdir().unwrap();
    let path = stage(tmp.path(), "alice-session/updates.jsonl", "grok.jsonl");
    let adapter = GrokAdapter::new().with_custom_root(tmp.path().to_path_buf());
    let t = parse(adapter.as_ref(), &path);
    assert_eq!(t.meta.tokens_used, Some(79));
    assert_eq!(
        t.mainline[1].usage,
        usage(Some(20), Some(7), Some(30), Some(10))
    );
    // Missing cache creation prevents inferring uncached input. The source's
    // prompt total remains usable, independently of the incomplete split.
    assert_eq!(t.mainline[3].usage, usage(None, Some(2), Some(4), None));
    assert!(t.mainline[3].text.is_empty());
    assert_eq!(t.mainline[5].usage, None);
}

#[test]
fn grok_meta_snapshots_do_not_invent_usage() {
    let tmp = tempfile::tempdir().unwrap();
    let path = stage(
        tmp.path(),
        "alice-session/updates.jsonl",
        "grok-snapshot.jsonl",
    );
    let adapter = GrokAdapter::new().with_custom_root(tmp.path().to_path_buf());
    let t = parse(adapter.as_ref(), &path);
    assert_eq!(t.meta.tokens_used, None);
    assert!(t.mainline.iter().all(|m| m.usage.is_none()));
}

#[test]
fn grok_tool_update_preserves_full_input_for_derivation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("alice-session/updates.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let input = "x".repeat(20_000);
    let rows = [
        json!({"params":{"update":{"sessionUpdate":"tool_call","toolCallId":"alice-call","title":"apply_patch"}}}),
        json!({"params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"alice-call","rawInput":{"patch":input},"status":"completed"}}}),
    ];
    fs::write(
        &path,
        rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    let adapter = GrokAdapter::new().with_custom_root(tmp.path().to_path_buf());
    let t = parse(adapter.as_ref(), &path);
    let saved: serde_json::Value =
        serde_json::from_str(t.mainline[0].tool_calls[0].input.as_deref().unwrap()).unwrap();
    assert_eq!(saved["patch"], input);
}

#[test]
fn hermes_structured_failures_preserve_call_mapping_and_unknown_usage() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(&fs::read_to_string(fixture("hermes.sql")).unwrap())
        .unwrap();
    drop(db);
    let adapter = HermesAdapter::new().with_custom_root(tmp.path().to_path_buf());
    let refs = adapter.list_session_files().unwrap();
    assert_eq!(refs.len(), 1);
    let t = adapter.parse_transcript(&refs[0]).unwrap();
    assert_eq!(t.meta.tokens_used, Some(12));
    assert_eq!(t.mainline[0].role, Role::Assistant);
    assert_eq!(t.mainline[0].usage, None);
    let calls = &t.mainline[0].tool_calls;
    assert_eq!(calls.len(), 8);
    for call in calls {
        assert_eq!(
            call.is_error,
            matches!(call.id.as_str(), "error" | "success" | "exit"),
            "{}",
            call.id
        );
    }
}

#[test]
fn grok_completion_without_counters_stays_an_empty_shell() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("alice-session/updates.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "{\"params\":{\"update\":{\"sessionUpdate\":\"turn_completed\",\"usage\":{}}}}\n",
    )
    .unwrap();
    let adapter = GrokAdapter::new().with_custom_root(tmp.path().to_path_buf());
    assert!(adapter.file_ref(&path).is_none());
}

#[test]
fn hermes_status_and_wholly_failed_batches_match_source_semantics() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("state.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(&fs::read_to_string(fixture("hermes.sql")).unwrap())
        .unwrap();
    let cases = [
        (
            json!({"results":[{"status":"error","error":"Synthetic child failed"}]}),
            true,
        ),
        (json!({"status":"FAILED"}), true),
        (json!({"status":"error"}), true),
        (json!({"status":"timeout"}), false),
        (
            json!({"results":[{"error":"Synthetic child failed"},{"status":"completed"}]}),
            false,
        ),
        (json!({"results":[]}), false),
        (json!({"results":[{"error":null}]}), false),
        (json!({"result":{"error":"Ordinary document field"}}), false),
        (json!({"guardrail_refusal":true,"status":"failed"}), false),
    ];
    for (i, (output, _)) in cases.iter().enumerate() {
        let id = format!("alice-batch-{i}");
        db.execute("INSERT INTO messages(session_id,role,tool_calls) VALUES('alice-hermes','assistant',?1)",
            [json!([{"id":id,"name":"delegate_task","arguments":{}}]).to_string()]).unwrap();
        db.execute("INSERT INTO messages(session_id,role,content,tool_call_id,tool_name) VALUES('alice-hermes','tool',?1,?2,'delegate_task')",
            [output.to_string(), id]).unwrap();
    }
    drop(db);
    let adapter = HermesAdapter::new().with_custom_root(tmp.path().to_path_buf());
    let t = adapter
        .parse_transcript(&adapter.list_session_files().unwrap()[0])
        .unwrap();
    for (i, (_, expected)) in cases.iter().enumerate() {
        let id = format!("alice-batch-{i}");
        let call = t
            .mainline
            .iter()
            .flat_map(|m| &m.tool_calls)
            .find(|c| c.id == id)
            .unwrap();
        assert_eq!(call.is_error, *expected, "{id}");
    }
}
