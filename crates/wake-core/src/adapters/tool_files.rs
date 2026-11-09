//! File operands from explicit file tools, before their inputs are clipped.
use crate::models::{AgentId, ToolCallView, ToolFile};
use serde_json::Value;

pub(super) fn from_call(
    agent: AgentId,
    call: &ToolCallView,
    idx: usize,
    cwd: &str,
) -> Vec<ToolFile> {
    let Some(input) = call.full_input.as_deref().or(call.input.as_deref()) else {
        return Vec::new();
    };
    let value = serde_json::from_str::<Value>(input).ok();
    let mut paths = Vec::new();
    match call.name.as_str() {
        "apply_patch" => patch_paths(
            value.as_ref().and_then(Value::as_str).unwrap_or(input),
            &mut paths,
        ),
        "patch" => {
            if let Some(patch) = value
                .as_ref()
                .and_then(|v| v.get("patch"))
                .and_then(Value::as_str)
            {
                patch_paths(patch, &mut paths);
            } else if let Some(path) = field(value.as_ref(), &["path"]) {
                paths.push((path.to_owned(), "change"));
            }
        }
        name => {
            let (fields, op): (&[&str], _) = match name {
                "Edit" | "Write" => (&["file_path"], "change"),
                "Read" => (&["file_path"], "read"),
                "ReadFile" => (&["path"], "read"),
                "NotebookEdit" => (&["notebook_path"], "change"),
                "read" => (&["file_path", "path", "filePath"], "read"),
                "edit" | "write" => (&["file_path", "path", "filePath"], "change"),
                "read_file" => (&["path", "Path"], "read"),
                "write_file" => (&["path"], "change"),
                "view_image" => (&["path"], "read"),
                "view_file" | "client_view_file" => (&["AbsolutePath", "absolute_path"], "read"),
                "replace_file_content"
                | "write_to_file"
                | "client_create_file"
                | "client_edit_file" => (&["TargetFile", "target_file"], "change"),
                _ => return Vec::new(),
            };
            if let Some(path) = field(value.as_ref(), fields) {
                // Antigravity serializes each argument value as JSON inside the args object.
                let path = if agent == AgentId::Antigravity {
                    serde_json::from_str::<String>(path).unwrap_or_else(|_| path.to_owned())
                } else {
                    path.to_owned()
                };
                paths.push((path, op));
            }
        }
    }
    let mut files = Vec::new();
    for (path, op) in paths {
        let file = ToolFile {
            idx,
            path: resolve(&path, cwd),
            op,
        };
        if !files.contains(&file) {
            files.push(file);
        }
    }
    files
}

fn field<'a>(value: Option<&'a Value>, names: &[&str]) -> Option<&'a str> {
    let value = value?;
    names
        .iter()
        .find_map(|name| value.get(name).and_then(Value::as_str))
}

fn patch_paths(patch: &str, paths: &mut Vec<(String, &'static str)>) {
    let mut update = None;
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("*** Move to: ") {
            if let Some(old) = update.take() {
                paths.push((old, "delete"));
                paths.push((path.to_owned(), "change"));
            }
        } else if line.starts_with("*** Add File: ")
            || line.starts_with("*** Update File: ")
            || line.starts_with("*** Delete File: ")
            || line == "*** End Patch"
        {
            if let Some(path) = update.take() {
                paths.push((path, "change"));
            }
            if let Some(path) = line.strip_prefix("*** Add File: ") {
                paths.push((path.to_owned(), "change"));
            } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
                paths.push((path.to_owned(), "delete"));
            } else if let Some(path) = line.strip_prefix("*** Update File: ") {
                update = Some(path.to_owned());
            }
        }
    }
    if let Some(path) = update {
        paths.push((path, "change"));
    }
}

/// Pure lexical resolution: no canonicalize, stat, or symlink traversal.
fn resolve(path: &str, cwd: &str) -> String {
    if cwd.is_empty() {
        return path.to_owned();
    }
    // Transcripts can come from another OS. Interpret the source path syntax,
    // not the host's Path::components rules (notably POSIX paths on Windows).
    let windows_drive =
        |s: &str| matches!(s.as_bytes(), [d, b':', b'/' | b'\\', ..] if d.is_ascii_alphabetic());
    let drive_relative =
        matches!(path.as_bytes(), [d, b':', ..] if d.is_ascii_alphabetic()) && !windows_drive(path);
    let path = if drive_relative {
        if windows_drive(cwd) && path[..1].eq_ignore_ascii_case(&cwd[..1]) {
            &path[2..]
        } else {
            // Another drive's current directory is not supplied by this session.
            return path.to_owned();
        }
    } else {
        path
    };
    let windows = windows_drive(path)
        || path.starts_with("\\\\")
        || windows_drive(cwd)
        || cwd.starts_with("\\\\");
    let (path, cwd) = if windows {
        (path.replace('\\', "/"), cwd.replace('\\', "/"))
    } else {
        (path.to_owned(), cwd.to_owned())
    };
    let joined = if windows && path.starts_with('/') && !path.starts_with("//") {
        // A rooted Windows operand keeps the cwd's drive or UNC share.
        let root = if windows_drive(&cwd) {
            &cwd[..2]
        } else {
            let end = cwd.match_indices('/').nth(3).map_or(cwd.len(), |(i, _)| i);
            &cwd[..end]
        };
        format!("{root}{path}")
    } else if path.starts_with('/') || windows_drive(&path) {
        path
    } else {
        format!("{cwd}/{path}")
    };
    let (prefix, rest, floor) = if windows_drive(&joined) {
        (format!("{}/", &joined[..2]), &joined[3..], 0)
    } else if windows && joined.starts_with("//") {
        ("//".to_owned(), &joined[2..], 2)
    } else if let Some(rest) = joined.strip_prefix('/') {
        ("/".to_owned(), rest, 0)
    } else {
        (String::new(), joined.as_str(), 0)
    };
    let mut parts = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.len() > floor && parts.last() != Some(&"..") => {
                parts.pop();
            }
            ".." if prefix.is_empty() => parts.push(part),
            ".." => {}
            _ => parts.push(part),
        }
    }
    let result = format!("{prefix}{}", parts.join("/"));
    if windows {
        result.replace('/', "\\")
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::parse_utils::tool_call_view;
    use serde_json::json;

    fn files(agent: AgentId, name: &str, input: Value) -> Vec<ToolFile> {
        let call = tool_call_view("call".into(), name, &input, None, false);
        from_call(agent, &call, 3, "/home/alice/project")
    }

    #[test]
    fn explicit_file_tools_and_encoded_antigravity_paths() {
        for (agent, name, input, op) in [
            (
                AgentId::ClaudeCode,
                "Edit",
                json!({"file_path":"src/../edit.rs"}),
                "change",
            ),
            (
                AgentId::ClaudeCode,
                "Write",
                json!({"file_path":"edit.rs"}),
                "change",
            ),
            (
                AgentId::ClaudeCode,
                "Read",
                json!({"file_path":"edit.rs"}),
                "read",
            ),
            (
                AgentId::Antigravity,
                "view_file",
                json!({"AbsolutePath":"\"/home/alice/project/edit.rs\""}),
                "read",
            ),
            (
                AgentId::Antigravity,
                "client_edit_file",
                json!({"target_file":"\"edit.rs\""}),
                "change",
            ),
            (
                AgentId::Antigravity,
                "write_to_file",
                json!({"TargetFile":"edit.rs"}),
                "change",
            ),
            (
                AgentId::Antigravity,
                "read_file",
                json!({"Path":"\"edit.rs\""}),
                "read",
            ),
            (
                AgentId::Hermes,
                "patch",
                json!({"path":"edit.rs", "old_string":"a", "new_string":"b"}),
                "change",
            ),
        ] {
            assert_eq!(
                files(agent, name, input),
                vec![ToolFile {
                    idx: 3,
                    path: "/home/alice/project/edit.rs".into(),
                    op
                }]
            );
        }
    }

    #[test]
    fn patch_collects_all_operations_before_clipping() {
        let patch = format!("*** Begin Patch\n*** Add File: added.rs\n+{}\n*** Update File: old.rs\n*** Move to: moved.rs\n@@\n-a\n+b\n*** Delete File: deleted.rs\n*** Update File: last.rs\n@@\n-a\n+b\n*** End Patch", "x".repeat(17 * 1024));
        let result = files(AgentId::Codex, "apply_patch", json!(patch));
        let pairs: Vec<_> = result.iter().map(|f| (f.path.as_str(), f.op)).collect();
        assert_eq!(
            pairs,
            vec![
                ("/home/alice/project/added.rs", "change"),
                ("/home/alice/project/old.rs", "delete"),
                ("/home/alice/project/moved.rs", "change"),
                ("/home/alice/project/deleted.rs", "delete"),
                ("/home/alice/project/last.rs", "change"),
            ]
        );
        assert_eq!(
            files(AgentId::Hermes, "patch", json!({"patch":patch})),
            result
        );
    }

    #[test]
    fn shell_and_resource_tools_do_not_imply_file_operations() {
        for name in [
            "Bash",
            "bash",
            "exec",
            "exec_command",
            "terminal",
            "run_command",
            "mcp__example__write_file",
            "search_files",
        ] {
            assert!(files(
                AgentId::ClaudeCode,
                name,
                json!({"path":"edit.rs", "file_path":"edit.rs", "command":"cat edit.rs > new.rs"})
            )
            .is_empty());
        }
    }

    #[test]
    fn paths_without_cwd_are_kept_and_relative_paths_are_lexical() {
        assert_eq!(resolve("./src/../file.rs", ""), "./src/../file.rs");
        assert_eq!(
            resolve("../../../../file.rs", "/home/alice/project"),
            "/file.rs"
        );
        assert_eq!(
            resolve("../file.rs", "/home/alice/project"),
            "/home/alice/file.rs"
        );
    }
    #[test]
    fn source_windows_paths_are_resolved_on_any_host() {
        assert_eq!(
            resolve(r"src\..\file.rs", r"C:\Users\alice\project"),
            r"C:\Users\alice\project\file.rs"
        );
        assert_eq!(
            resolve(r"D:\files\..\file.rs", r"C:\Users\alice"),
            r"D:\file.rs"
        );
        assert_eq!(
            resolve(r"..\file.rs", r"\\server\share\project"),
            r"\\server\share\file.rs"
        );
    }
    #[test]
    fn windows_root_relative_paths_inherit_the_cwd_volume() {
        assert_eq!(
            resolve(r"\tmp\alice.txt", r"C:\Users\alice\project"),
            r"C:\tmp\alice.txt"
        );
        assert_eq!(
            resolve("/tmp/alice.txt", r"C:\Users\alice\project"),
            r"C:\tmp\alice.txt"
        );
        assert_eq!(
            resolve(r"\tmp\alice.txt", r"\\server\share\project"),
            r"\\server\share\tmp\alice.txt"
        );
    }
    #[test]
    fn windows_drive_relative_paths_use_only_the_known_drive_cwd() {
        let cwd = r"C:\Users\alice\project";
        assert_eq!(
            resolve("C:alice.txt", cwd),
            r"C:\Users\alice\project\alice.txt"
        );
        assert_eq!(resolve(r"c:..\alice.txt", cwd), r"C:\Users\alice\alice.txt");
        assert_eq!(resolve("C:", cwd), cwd);
        assert_eq!(resolve("D:alice.txt", cwd), "D:alice.txt");
        assert_eq!(resolve("C:alice.txt", ""), "C:alice.txt");
        assert_eq!(resolve("C:alice.txt", "/home/alice/project"), "C:alice.txt");
    }
    #[test]
    fn source_specific_file_names_and_arguments_are_recognized() {
        for (agent, name, input, op) in [
            (AgentId::Kimi, "ReadFile", json!({"path":"file.rs"}), "read"),
            (
                AgentId::Opencode,
                "edit",
                json!({"filePath":"file.rs"}),
                "change",
            ),
            (
                AgentId::Opencode,
                "read",
                json!({"filePath":"file.rs"}),
                "read",
            ),
            (
                AgentId::ClaudeCode,
                "NotebookEdit",
                json!({"notebook_path":"file.rs"}),
                "change",
            ),
        ] {
            assert_eq!(
                files(agent, name, input),
                vec![ToolFile {
                    idx: 3,
                    path: "/home/alice/project/file.rs".into(),
                    op
                }]
            );
        }
    }
}
