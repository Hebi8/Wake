//! Synthetic usage and process-status contracts; no native stores are opened.
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use wake_core::adapters::{claude::ClaudeAdapter, codex::CodexAdapter, AgentAdapter};
use wake_core::models::{MessageKind, ParsedTranscript, TokenUsage};

fn write_rows(path: &Path, rows: &[Value]) {
    fs::write(
        path,
        rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
}

fn parse(agent: &str, rows: &[Value]) -> ParsedTranscript {
    let root = tempfile::tempdir().unwrap();
    let path = if agent == "claude" {
        root.path().join("projects/-home-alice-project/alice.jsonl")
    } else {
        root.path().join("rollout-alice.jsonl")
    };
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_rows(&path, rows);
    let adapter = adapter(agent, root.path());
    let file = adapter.file_ref(&path).unwrap();
    let transcript = adapter.parse_transcript(&file).unwrap();
    let indexed = adapter.parse_session(&file).unwrap();
    assert_eq!(indexed.meta.tokens_used, transcript.meta.tokens_used);
    assert_eq!(transcript.unknown_line_count, 0);
    transcript
}

fn indexed_codex(rows: &[Value]) -> wake_core::models::ParsedSession {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("rollout-alice.jsonl");
    write_rows(&path, rows);
    let adapter = adapter("codex", root.path());
    adapter
        .parse_session(&adapter.file_ref(&path).unwrap())
        .unwrap()
}

fn adapter(agent: &str, root: &Path) -> Box<dyn AgentAdapter> {
    match agent {
        "claude" => ClaudeAdapter::new().with_custom_root(root.to_path_buf()),
        "codex" => CodexAdapter::new().with_custom_root(root.to_path_buf()),
        _ => unreachable!(),
    }
}

fn fixture(name: &str) -> Vec<Value> {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/usage_core")
            .join(name),
    )
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect()
}

fn usages(t: &ParsedTranscript) -> Vec<TokenUsage> {
    t.mainline.iter().filter_map(|m| m.usage).collect()
}

fn usage(input: i64, output: i64, read: i64, write: Option<i64>) -> TokenUsage {
    TokenUsage {
        input: Some(input),
        output: Some(output),
        cache_read: Some(read),
        cache_write: write,
    }
}

fn claude(id: Option<&str>, text: &str, usage: Value) -> Value {
    let mut row = json!({"type":"assistant","message":{"role":"assistant","model":"claude-example","content":text,"usage":usage}});
    if let Some(id) = id {
        row["message"]["id"] = json!(id);
    }
    row
}

#[test]
fn claude_three_blocks_keep_cache_read_separate_from_session_total() {
    let t = parse("claude", &fixture("claude_three_blocks.jsonl"));
    assert_eq!(t.mainline.len(), 1);
    assert_eq!(usages(&t), [usage(100, 20, 300, Some(40))]);
    assert_eq!(t.meta.tokens_used, Some(160));
    assert!(t.mainline[0].thinking.is_some());
    assert_eq!(t.mainline[0].tool_calls.len(), 1);
}

#[test]
fn claude_differing_usage_uses_last_snapshot_not_maxima() {
    let t = parse("claude", &fixture("claude_last_wins.jsonl"));
    assert_eq!(usages(&t), [usage(90, 5, 200, Some(10))]);
    assert_eq!(t.meta.tokens_used, Some(105));
}

#[test]
fn claude_last_zero_snapshot_replaces_nonzero_usage() {
    let t = parse("claude", &fixture("claude_last_zero.jsonl"));
    assert_eq!(usages(&t), [usage(0, 0, 0, Some(0))]);
    assert_eq!(t.meta.tokens_used, Some(0));
}

#[test]
fn claude_nonadjacent_api_id_has_one_owner_and_last_fields() {
    let t = parse(
        "claude",
        &[
            claude(
                Some("a"),
                "First block",
                json!({"input_tokens":10,"output_tokens":2}),
            ),
            claude(
                Some("b"),
                "Another response",
                json!({"input_tokens":20,"output_tokens":3}),
            ),
            claude(Some("a"), "Repeated block", json!({"output_tokens":1})),
        ],
    );
    assert_eq!(t.mainline.len(), 3);
    assert_eq!(
        t.mainline[0].usage,
        Some(TokenUsage {
            output: Some(1),
            ..Default::default()
        })
    );
    assert!(t.mainline[2].usage.is_none());
    assert_eq!(t.meta.tokens_used, Some(24));
}

#[test]
fn claude_missing_ids_keep_existing_merge_and_per_record_counting() {
    let t = parse(
        "claude",
        &[
            claude(
                None,
                "First block",
                json!({"input_tokens":10,"output_tokens":2}),
            ),
            claude(
                None,
                "Second block",
                json!({"input_tokens":20,"output_tokens":3}),
            ),
        ],
    );
    assert_eq!(t.mainline.len(), 1);
    assert_eq!(
        usages(&t),
        [TokenUsage {
            input: Some(30),
            output: Some(5),
            ..Default::default()
        }]
    );
    assert_eq!(t.meta.tokens_used, Some(35));
}

#[test]
fn claude_usage_only_and_unknown_fields_are_preserved() {
    let t = parse(
        "claude",
        &[
            claude(Some("a"), "", json!({"output_tokens":7})),
            claude(Some("b"), "No usage", Value::Null),
        ],
    );
    assert_eq!(t.mainline.len(), 1);
    assert_eq!(
        usages(&t),
        [TokenUsage {
            output: Some(7),
            ..Default::default()
        }]
    );
    assert_eq!(t.meta.tokens_used, Some(7));
}

#[test]
fn claude_inherited_prefix_excludes_usage_but_continued_block_is_owned() {
    for continued in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("projects/-home-alice-project");
        fs::create_dir_all(&project).unwrap();
        let original = project.join("alice-original.jsonl");
        let branch = project.join("alice-branch.jsonl");
        let mut shared = fixture("claude_three_blocks.jsonl");
        let mut first = vec![json!({"type":"queue-operation","timestamp":"2026-01-01T00:00:00Z"})];
        first.extend(shared.clone());
        write_rows(&original, &first);
        let mut fork = vec![json!({"type":"queue-operation","timestamp":"2026-01-02T00:00:00Z"})];
        fork.append(&mut shared);
        if !continued {
            fork.push(json!({"type":"user","uuid":"alice-new-user","message":{"role":"user","content":"Continue the sample."}}));
        }
        let mut own = claude(
            Some(if continued { "api-a" } else { "api-b" }),
            "Branch response",
            json!({"input_tokens":4,"output_tokens":2}),
        );
        own["uuid"] = json!("alice-own-block");
        fork.push(own);
        write_rows(&branch, &fork);
        let adapter = adapter("claude", root.path());
        let t = adapter
            .parse_transcript(&adapter.file_ref(&branch).unwrap())
            .unwrap();
        assert_eq!(
            usages(&t),
            [TokenUsage {
                input: Some(4),
                output: Some(2),
                ..Default::default()
            }]
        );
        assert_eq!(t.meta.tokens_used, Some(6));
        if !continued {
            assert_eq!(t.mainline[0].kind, MessageKind::Meta);
        }
    }
}

fn item(payload: Value) -> Value {
    json!({"type":"response_item","payload":payload})
}
fn event(payload: Value) -> Value {
    json!({"type":"event_msg","payload":payload})
}
fn text(role: &str, text: &str) -> Value {
    item(json!({"type":"message","role":role,"content":[{"type":"output_text","text":text}]}))
}
fn thinking(text: &str) -> Value {
    item(json!({"type":"reasoning","summary":[{"type":"summary_text","text":text}]}))
}
fn call(id: &str) -> Value {
    item(
        json!({"type":"function_call","call_id":id,"name":"exec_command","arguments":"{\"cmd\":\"pwd\"}"}),
    )
}
fn source_usage(input: i64, output: i64, cached: i64) -> Value {
    json!({"input_tokens":input,"output_tokens":output,"cached_input_tokens":cached,"reasoning_output_tokens":output/2,"total_tokens":input+output})
}
fn count(usage: Value, total: i64) -> Value {
    event(
        json!({"type":"token_count","info":{"last_token_usage":usage,"total_token_usage":{"total_tokens":total}},"rate_limits":null}),
    )
}
fn current(id: &str, usage: Value, total: i64) -> Value {
    json!({"type":"token_usage_record","payload":{"thread_id":"alice-thread","turn_id":"alice-turn","session_id":"alice-thread","root_turn_id":"alice-turn","response_id":id,"usage":usage,"turn_token_usage":{"total_tokens":total},"thread_token_usage":{"total_tokens":total}}})
}

#[test]
fn codex_legacy_usage_follows_responses_across_thinking_and_tool_blocks() {
    let t = parse(
        "codex",
        &[
            text("user", "Inspect the sample"),
            thinking("First reasoning"),
            text("assistant", "Read the file"),
            call("a"),
            call("b"),
            count(source_usage(100, 20, 60), 120),
            item(
                json!({"type":"function_call_output","call_id":"a","output":"Wall time: 0.1 seconds\nProcess exited with code 0\nOutput:\n/home/alice"}),
            ),
            thinking("Second reasoning"),
            call("c"),
            count(source_usage(150, 30, 100), 300),
            text("assistant", "Finished"),
            count(source_usage(200, 40, 150), 540),
        ],
    );
    assert_eq!(
        usages(&t),
        [
            usage(40, 20, 60, None),
            usage(50, 30, 100, None),
            usage(50, 40, 150, None)
        ]
    );
    let first = t
        .mainline
        .iter()
        .find(|m| m.text == "Read the file")
        .unwrap();
    assert_eq!(first.tool_calls.len(), 2);
    assert!(first.tool_calls[0].output.is_some());
    let second = t
        .mainline
        .iter()
        .find(|m| m.thinking.as_deref() == Some("Second reasoning"))
        .unwrap();
    assert_eq!(second.tool_calls.len(), 1);
    assert_eq!(second.usage, Some(usage(50, 30, 100, None)));
    assert_eq!(t.meta.tokens_used, Some(540));
}

#[test]
fn codex_unchanged_count_snapshots_do_not_consume_the_next_response() {
    let t = parse(
        "codex",
        &[
            text("assistant", "First"),
            count(source_usage(100, 20, 60), 120),
            count(source_usage(100, 20, 60), 120),
            thinking("Next response"),
            count(source_usage(99, 21, 50), 120),
            call("next"),
            count(source_usage(100, 20, 60), 240),
        ],
    );
    assert_eq!(
        usages(&t),
        [usage(40, 20, 60, None), usage(40, 20, 60, None)]
    );
    assert_eq!(t.mainline.last().unwrap().tool_calls.len(), 1);
    assert_eq!(t.meta.tokens_used, Some(240));
}

#[test]
fn codex_current_records_and_legacy_stream_are_not_double_counted() {
    let mut u = source_usage(100, 20, 60);
    u["cache_write_input_tokens"] = json!(15);
    let t = parse(
        "codex",
        &[
            text("assistant", "Older response"),
            count(source_usage(10, 2, 0), 12),
            thinking("Current response"),
            call("current"),
            current("r1", u.clone(), 132),
            count(source_usage(95, 18, 55), 132),
            count(u.clone(), 132),
            text("assistant", "Final response"),
            current("r2", source_usage(200, 30, 100), 362),
            count(source_usage(200, 30, 100), 362),
        ],
    );
    assert_eq!(
        usages(&t),
        [
            usage(10, 2, 0, None),
            usage(25, 20, 60, Some(15)),
            usage(100, 30, 100, None)
        ]
    );
    assert_eq!(usages(&t).iter().map(|u| u.total()).sum::<i64>(), 362);
    assert_eq!(t.meta.tokens_used, Some(362));
}

#[test]
fn codex_usage_only_source_keeps_total_without_display_rows() {
    let t = parse(
        "codex",
        &[
            current("r1", source_usage(100, 20, 60), 120),
            current("r2", source_usage(100, 20, 60), 240),
        ],
    );
    assert!(t.mainline.is_empty());
    assert!(usages(&t).is_empty());
    assert_eq!(t.meta.tokens_used, Some(240));
}

#[test]
fn codex_nonadjacent_current_response_id_has_one_owner() {
    let t = parse(
        "codex",
        &[
            text("assistant", "First"),
            current("r1", source_usage(100, 20, 60), 120),
            text("assistant", "Second"),
            current("r2", source_usage(200, 30, 100), 350),
            current("r1", source_usage(100, 20, 60), 350),
        ],
    );
    assert_eq!(t.mainline.len(), 2);
    assert_eq!(
        usages(&t),
        [usage(40, 20, 60, None), usage(100, 30, 100, None)]
    );
}

#[test]
fn codex_event_only_history_keeps_response_usage() {
    let t = parse(
        "codex",
        &[
            event(json!({"type":"user_message","message":"Inspect the sample"})),
            event(json!({"type":"agent_message","message":"First"})),
            count(source_usage(100, 20, 60), 120),
            event(json!({"type":"agent_message","message":"Second"})),
            current("r2", source_usage(200, 30, 100), 350),
            count(source_usage(200, 30, 100), 350),
        ],
    );
    assert_eq!(t.mainline.len(), 3);
    assert_eq!(t.mainline[1].text, "First");
    assert_eq!(t.mainline[2].text, "Second");
    assert_eq!(
        usages(&t),
        [usage(40, 20, 60, None), usage(100, 30, 100, None)]
    );
}

#[test]
fn codex_inherited_prefix_excludes_message_usage() {
    for ordinal_boundary in [true, false] {
        let mut meta = json!({"type":"session_meta","payload":{"cwd":"/home/alice/project","source":{"subagent":{"thread_spawn":{"agent_path":"/root/alice","agent_nickname":"alice"}}}}});
        if ordinal_boundary {
            meta["payload"]["subagent_history_start_ordinal"] = json!(10);
        }
        let mut own = call("own");
        own["ordinal"] = json!(10);
        let t = parse(
            "codex",
            &[
                meta,
                text("assistant", "Inherited"),
                current("parent", source_usage(100, 20, 60), 120),
                count(source_usage(100, 20, 60), 120),
                item(json!({"type":"agent_message","recipient":"/root/alice"})),
                own,
                current("child", source_usage(50, 10, 20), 180),
                count(source_usage(50, 10, 20), 180),
            ],
        );
        assert_eq!(t.mainline[0].kind, MessageKind::Meta);
        assert_eq!(usages(&t), [usage(30, 10, 20, None)]);
        assert_eq!(t.mainline[1].tool_calls[0].id, "own");
        // The copied parent is excluded from the session total as well.
        assert_eq!(t.meta.tokens_used, Some(60));
    }
}

#[test]
fn codex_unknown_usage_fields_stay_unknown_and_reasoning_is_not_added() {
    let t = parse(
        "codex",
        &[
            current(
                "r1",
                json!({"output_tokens":8,"reasoning_output_tokens":3}),
                8,
            ),
            text("assistant", "No usage"),
            current("r2", Value::Null, 8),
        ],
    );
    assert_eq!(
        usages(&t),
        [TokenUsage {
            output: Some(8),
            ..Default::default()
        }]
    );
}

#[test]
fn codex_process_status_accepts_only_structured_fields_and_result_headers() {
    let cases = [
        (json!({"output":"sample","metadata":{"exit_code":2}}),true),
        (json!({"output":"error text","metadata":{"exit_code":0}}),false),


        (json!("{\"output\":\"sample\",\"metadata\":{\"exit_code\":3}}"),true),
        (json!("Chunk ID: sample\nWall time: 0.1 seconds\nProcess exited with code 1\nOutput:\nsample"),true),
        (json!("Wall time: 0.1 seconds\nExit code: 0\nOutput:\nProcess exited with code 1"),false),

        (json!("Wall time: 0.1 seconds\nProcess running with session ID 42\nOutput:\nProcess exited with code 1"),false),
        (json!("error: exit code 1"),false),
        (json!("Process exited with code 1"),false),
    ];
    for (output, is_error) in cases {
        let t = indexed_codex(&[
            call("exec"),
            item(json!({"type":"function_call_output","call_id":"exec","output":output})),
        ]);
        assert_eq!(t.structure[0].tool_calls[0].is_error, is_error, "{output}");
    }
}

#[test]
fn codex_full_raw_input_reaches_derive() {
    let patch = format!("*** Begin Patch\n*** Add File: /home/alice/project/first.txt\n+{}\n*** Add File: /home/alice/project/last.txt\n+sample\n*** End Patch", "a".repeat(20_000));
    let t = parse(
        "codex",
        &[item(
            json!({"type":"custom_tool_call","call_id":"patch","name":"apply_patch","input":patch}),
        )],
    );
    assert_eq!(
        t.mainline[0].tool_calls[0].full_input.as_deref(),
        Some(patch.as_str())
    );
}

#[test]
fn claude_empty_or_synthetic_usage_does_not_invent_tokens() {
    let mut synthetic = claude(Some("synthetic"), "Interrupted", json!({"input_tokens":3}));
    synthetic["message"]["model"] = json!("<synthetic>");
    let t = parse(
        "claude",
        &[claude(Some("a"), "Unknown usage", json!({})), synthetic],
    );
    assert!(usages(&t).is_empty());
    assert_eq!(t.meta.tokens_used, None);
}

#[test]
fn codex_usage_only_current_record_after_legacy_is_a_distinct_response() {
    let t = parse(
        "codex",
        &[
            text("assistant", "First"),
            count(source_usage(100, 20, 60), 120),
            current("r2", source_usage(50, 10, 20), 180),
            count(source_usage(50, 10, 20), 180),
        ],
    );
    assert_eq!(t.mainline.len(), 1);
    assert_eq!(t.mainline[0].text, "First");
    assert_eq!(usages(&t), [usage(70, 30, 80, None)]);
}

#[test]
fn codex_initial_zero_and_null_info_are_snapshots_not_responses() {
    let t = parse(
        "codex",
        &[
            count(source_usage(0, 0, 0), 0),
            event(json!({"type":"token_count","info":null,"rate_limits":{}})),
            text("assistant", "First"),
            count(source_usage(100, 20, 60), 120),
        ],
    );
    assert_eq!(t.mainline.len(), 1);
    assert_eq!(usages(&t), [usage(40, 20, 60, None)]);
}

#[test]
fn codex_current_zero_usage_is_known_zero() {
    let t = parse(
        "codex",
        &[
            current("r1", source_usage(0, 0, 0), 0),
            count(source_usage(0, 0, 0), 0),
            count(source_usage(0, 0, 0), 0),
        ],
    );
    assert!(t.mainline.is_empty());
    assert!(usages(&t).is_empty());
    assert_eq!(t.meta.tokens_used, Some(0));
}

#[test]
fn codex_responses_keep_the_original_shared_tool_host() {
    let t = parse(
        "codex",
        &[
            call("first"),
            current("r1", source_usage(100, 20, 60), 120),
            count(source_usage(100, 20, 60), 120),
            count(source_usage(50, 10, 20), 180),
            call("next"),
            count(source_usage(50, 10, 20), 240),
        ],
    );
    assert_eq!(t.mainline.len(), 1);
    assert_eq!(t.mainline[0].tool_calls.len(), 2);
    assert_eq!(t.mainline[0].tool_calls[0].id, "first");
    assert_eq!(t.mainline[0].tool_calls[1].id, "next");
    assert_eq!(usages(&t), [usage(100, 40, 100, None)]);
}

#[test]
fn codex_total_only_source_keeps_its_reported_session_total() {
    let t = parse(
        "codex",
        &[
            text("assistant", "Legacy response without call counters"),
            event(json!({"type":"token_count","info":{"total_token_usage":{"total_tokens":120}}})),
        ],
    );
    assert!(usages(&t).is_empty());
    assert_eq!(t.meta.tokens_used, Some(120));
}

#[test]
fn codex_fork_with_only_inherited_usage_has_zero_owned_tokens() {
    let t = parse(
        "codex",
        &[
            json!({"type":"session_meta","payload":{"cwd":"/home/alice/project","source":{"subagent":{"thread_spawn":{"agent_path":"/root/alice"}}}}}),
            text("assistant", "Inherited response"),
            current("parent", source_usage(100, 20, 60), 120),
            count(source_usage(100, 20, 60), 120),
            item(json!({"type":"agent_message","recipient":"/root/alice"})),
        ],
    );
    assert!(usages(&t).is_empty());
    assert_eq!(t.meta.tokens_used, Some(0));
}

#[test]
fn codex_response_replay_after_count_replaces_usage_without_splitting_display() {
    let t = parse(
        "codex",
        &[
            text("assistant", "Working."),
            call("first"),
            current("r1", source_usage(100, 20, 60), 120),
            count(source_usage(100, 20, 60), 120),
            current("r1", source_usage(110, 20, 60), 130),
            count(source_usage(110, 20, 60), 130),
            call("second"),
            current("r2", source_usage(10, 2, 0), 142),
            count(source_usage(10, 2, 0), 142),
        ],
    );
    assert_eq!(t.mainline.len(), 1);
    assert_eq!(t.mainline[0].tool_calls.len(), 2);
    assert_eq!(usages(&t), [usage(60, 22, 60, None)]);
    assert_eq!(t.meta.tokens_used, Some(142));
}

#[test]
fn codex_code_mode_errors_use_only_the_outer_script_status() {
    for name in ["exec", "wait"] {
        for (status, nested, expected) in
            [("Script completed", 1, false), ("Script failed", 0, true)]
        {
            let rows = [
                item(json!({"type":"function_call","name":name,"call_id":"code","arguments":"{}"})),
                item(
                    json!({"type":"function_call_output","call_id":"code","output":[
                        {"type":"text","text":status},
                        {"type":"text","text":format!("Wall time: 0.1 seconds\nProcess exited with code {nested}\nOutput:\nSynthetic output")}
                    ]}),
                ),
            ];
            assert_eq!(
                indexed_codex(&rows).structure[0].tool_calls[0].is_error,
                expected
            );
            assert!(!parse("codex", &rows).mainline[0].tool_calls[0].is_error);
        }
    }
}

#[test]
fn full_tool_input_and_usage_are_private_to_index_derivation() {
    let patch = format!(
        "*** Begin Patch\n*** Add File: first.txt\n+{}\n*** Add File: last.txt\n+x\n*** End Patch",
        "x".repeat(20_000)
    );
    let rows = [
        item(
            json!({"type":"custom_tool_call","name":"apply_patch","call_id":"patch","input":patch}),
        ),
        current("r1", source_usage(10, 2, 0), 12),
    ];
    let t = parse("codex", &rows);
    let call = &t.mainline[0].tool_calls[0];
    assert!(call.input.as_ref().unwrap().ends_with("… (truncated)"));
    assert_eq!(call.full_input.as_deref(), Some(patch.as_str()));
    let serialized = serde_json::to_value(&t.mainline[0]).unwrap();
    assert!(serialized.get("usage").is_none());
    assert!(serialized["tool_calls"][0].get("full_input").is_none());
    let indexed = indexed_codex(&rows);
    assert!(indexed.structure[0].tool_calls[0].full_input.is_none());
    assert!(indexed.structure[0]
        .tool_files
        .iter()
        .any(|f| f.path == "last.txt"));
}
