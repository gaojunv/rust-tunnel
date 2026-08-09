//! Tool definitions (JSON schema) and tool-call → AgentCommand conversion.
use rust_tunnel_common::AgentCommand;

/// OpenAI tools 格式的工具声明，透传给上游 LLM。
pub fn agent_tools_schema() -> Vec<serde_json::Value> {
    let file_props = |extra: &[(&str, serde_json::Value)]| {
        let mut props = serde_json::json!({
            "path": {"type": "string", "description": "Relative path within the workspace"}
        });
        for (k, v) in extra {
            props[k.to_string()] = v.clone();
        }
        props
    };
    vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run a shell command in the workspace. Returns stdout/stderr/exit_code.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cmd": {"type": "string", "description": "The shell command to run"},
                        "cwd": {"type": "string", "description": "Optional subdirectory of the workspace to run in"}
                    },
                    "required": ["cmd"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file's content from the workspace.",
                "parameters": {
                    "type": "object",
                    "properties": file_props(&[]),
                    "required": ["path"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write content to a file in the workspace (creates parent dirs).",
                "parameters": {
                    "type": "object",
                    "properties": file_props(&[("content", serde_json::json!({"type": "string", "description": "Full file content to write"}))]),
                    "required": ["path", "content"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List entries of a directory in the workspace (dirs end with '/').",
                "parameters": {
                    "type": "object",
                    "properties": file_props(&[]),
                    "required": ["path"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_status",
                "description": "Show git working-tree status of the workspace.",
                "parameters": {"type": "object", "properties": {}}
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_diff",
                "description": "Show git diff of the workspace, optionally limited to one file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Optional file to diff"}
                    }
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_commit",
                "description": "Stage all changes and commit with the given message.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "message": {"type": "string", "description": "Commit message"}
                    },
                    "required": ["message"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_push",
                "description": "Push the current branch to its upstream remote.",
                "parameters": {"type": "object", "properties": {}}
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "search",
                "description": "Search file contents with POSIX extended regex (ERE). Returns up to 200 lines of 'path:line:content'. Skips binary files and .git.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "ERE regex pattern"},
                        "path": {"type": "string", "description": "Starting directory relative to the workspace root ('.' for root)"},
                        "include": {"type": "string", "description": "Optional filename filter: '*.ext' suffix glob or exact filename"}
                    },
                    "required": ["pattern", "path"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "patch_file",
                "description": "Replace a unique anchor string in a file. old_string must occur EXACTLY ONCE in the file; on 0 or multiple matches the call fails — read_file first to confirm the exact current content.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Relative path within the workspace"},
                        "old_string": {"type": "string", "description": "Exact text to find (must appear exactly once)"},
                        "new_string": {"type": "string", "description": "Replacement text"}
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        }),
    ]
}

fn arg_str<'a>(args: &'a serde_json::Value, key: &str, tool: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("tool '{tool}' requires string argument '{key}'"))
}

fn arg_opt_str(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

/// Cap on a single tool input payload. The control-channel protocol cap is 1MB
/// and is enforced on the client's read side, where an oversize frame kills the
/// connection. Reject inputs below that cap here so an oversized write/shell cmd
/// becomes a model-facing error instead of tearing down the client.
const MAX_TOOL_INPUT: usize = 900 * 1024;

/// Convert an LLM function call into an AgentCommand. Errors are fed back to the model.
pub fn parse_tool_call(name: &str, args_json: &str) -> Result<AgentCommand, String> {
    let args: serde_json::Value =
        serde_json::from_str(args_json).map_err(|e| format!("invalid tool arguments: {e}"))?;
    match name {
        "shell" => {
            let cmd = arg_str(&args, "cmd", name)?;
            if cmd.len() > MAX_TOOL_INPUT {
                return Err("cmd too large (>900KB); split it into smaller commands".to_string());
            }
            Ok(AgentCommand::Shell {
                cmd: cmd.to_string(),
                cwd: arg_opt_str(&args, "cwd"),
            })
        }
        "read_file" => Ok(AgentCommand::ReadFile {
            path: arg_str(&args, "path", name)?.to_string(),
        }),
        "write_file" => {
            let path = arg_str(&args, "path", name)?;
            let content = arg_str(&args, "content", name)?;
            if content.len() > MAX_TOOL_INPUT {
                return Err(
                    "content too large (>900KB); write the file in smaller chunks".to_string(),
                );
            }
            Ok(AgentCommand::WriteFile {
                path: path.to_string(),
                content: content.to_string(),
            })
        }
        "list_dir" => Ok(AgentCommand::ListDir {
            path: arg_str(&args, "path", name)?.to_string(),
        }),
        "git_status" => Ok(AgentCommand::GitStatus),
        "git_diff" => Ok(AgentCommand::GitDiff {
            path: arg_opt_str(&args, "path"),
        }),
        "git_commit" => Ok(AgentCommand::GitCommit {
            message: arg_str(&args, "message", name)?.to_string(),
        }),
        "git_push" => Ok(AgentCommand::GitPush),
        "search" => {
            let pattern = arg_str(&args, "pattern", name)?;
            if pattern.len() > MAX_TOOL_INPUT {
                return Err("pattern too large (>900KB)".to_string());
            }
            Ok(AgentCommand::Search {
                pattern: pattern.to_string(),
                path: arg_str(&args, "path", name)?.to_string(),
                include: arg_opt_str(&args, "include"),
            })
        }
        "patch_file" => {
            let path = arg_str(&args, "path", name)?;
            let old_string = arg_str(&args, "old_string", name)?;
            let new_string = arg_str(&args, "new_string", name)?;
            if old_string.len() + new_string.len() > MAX_TOOL_INPUT {
                return Err("patch payload too large (>900KB); patch in smaller chunks".to_string());
            }
            Ok(AgentCommand::PatchFile {
                path: path.to_string(),
                old_string: old_string.to_string(),
                new_string: new_string.to_string(),
            })
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_covers_all_commands() {
        let schema = agent_tools_schema();
        let names: Vec<&str> = schema
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        for expected in [
            "shell",
            "read_file",
            "write_file",
            "list_dir",
            "git_status",
            "git_diff",
            "git_commit",
            "git_push",
        ] {
            assert!(names.contains(&expected), "missing tool: {expected}");
        }
    }

    #[test]
    fn test_parse_shell() {
        let cmd = parse_tool_call("shell", r#"{"cmd":"ls -la","cwd":"src"}"#).unwrap();
        match cmd {
            AgentCommand::Shell { cmd, cwd } => {
                assert_eq!(cmd, "ls -la");
                assert_eq!(cwd.as_deref(), Some("src"));
            }
            other => panic!("expected Shell, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_shell_cwd_optional() {
        let cmd = parse_tool_call("shell", r#"{"cmd":"pwd"}"#).unwrap();
        match cmd {
            AgentCommand::Shell { cwd, .. } => assert!(cwd.is_none()),
            other => panic!("expected Shell, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_write_file() {
        let cmd =
            parse_tool_call("write_file", r#"{"path":"a.rs","content":"fn main(){}"}"#).unwrap();
        match cmd {
            AgentCommand::WriteFile { path, content } => {
                assert_eq!(path, "a.rs");
                assert_eq!(content, "fn main(){}");
            }
            other => panic!("expected WriteFile, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_rejects_oversized_shell_cmd() {
        let big = "x".repeat(901 * 1024);
        let args = serde_json::json!({"cmd": big}).to_string();
        let err = parse_tool_call("shell", &args).unwrap_err();
        assert!(err.contains("too large"));
    }

    #[test]
    fn test_parse_rejects_oversized_write_content() {
        let big = "x".repeat(901 * 1024);
        let args = serde_json::json!({"path": "a.txt", "content": big}).to_string();
        let err = parse_tool_call("write_file", &args).unwrap_err();
        assert!(err.contains("too large"));
    }

    #[test]
    fn test_parse_accepts_just_under_limit() {
        let ok = "x".repeat(899 * 1024);
        let args = serde_json::json!({"cmd": ok}).to_string();
        assert!(parse_tool_call("shell", &args).is_ok());
    }

    #[test]
    fn test_parse_missing_required_arg() {
        assert!(parse_tool_call("read_file", r"{}").is_err());
        assert!(parse_tool_call("shell", r"{}").is_err());
    }

    #[test]
    fn test_parse_unknown_tool() {
        assert!(parse_tool_call("delete_everything", r"{}").is_err());
    }

    #[test]
    fn test_schema_covers_search_patch() {
        let schema = agent_tools_schema();
        let names: Vec<&str> = schema
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"search"));
        assert!(names.contains(&"patch_file"));
    }

    #[test]
    fn test_parse_search() {
        let cmd = parse_tool_call(
            "search",
            r#"{"pattern":"fn main","path":"src","include":"*.rs"}"#,
        )
        .unwrap();
        match cmd {
            AgentCommand::Search {
                pattern,
                path,
                include,
            } => {
                assert_eq!(pattern, "fn main");
                assert_eq!(path, "src");
                assert_eq!(include.as_deref(), Some("*.rs"));
            }
            other => panic!("expected Search, got {other:?}"),
        }
        // include 可选
        let cmd = parse_tool_call("search", r#"{"pattern":"x","path":"."}"#).unwrap();
        match cmd {
            AgentCommand::Search { include, .. } => assert!(include.is_none()),
            other => panic!("expected Search, got {other:?}"),
        }
        // 缺必填
        assert!(parse_tool_call("search", r#"{"pattern":"x"}"#).is_err());
    }

    #[test]
    fn test_parse_patch_file() {
        let cmd = parse_tool_call(
            "patch_file",
            r#"{"path":"a.rs","old_string":"o","new_string":"n"}"#,
        )
        .unwrap();
        match cmd {
            AgentCommand::PatchFile {
                path,
                old_string,
                new_string,
            } => {
                assert_eq!(path, "a.rs");
                assert_eq!(old_string, "o");
                assert_eq!(new_string, "n");
            }
            other => panic!("expected PatchFile, got {other:?}"),
        }
        assert!(parse_tool_call("patch_file", r#"{"path":"a.rs"}"#).is_err());
        // 超上限拒绝
        let big = "x".repeat(901 * 1024);
        let args =
            serde_json::json!({"path":"a.rs","old_string":"o","new_string": big}).to_string();
        assert!(parse_tool_call("patch_file", &args)
            .unwrap_err()
            .contains("too large"));
    }

    #[test]
    fn test_parse_invalid_json() {
        assert!(parse_tool_call("shell", "not json").is_err());
    }

    #[test]
    fn test_parse_git_variants() {
        assert!(matches!(
            parse_tool_call("git_status", r"{}").unwrap(),
            AgentCommand::GitStatus
        ));
        assert!(matches!(
            parse_tool_call("git_push", r"{}").unwrap(),
            AgentCommand::GitPush
        ));
        match parse_tool_call("git_commit", r#"{"message":"fix"}"#).unwrap() {
            AgentCommand::GitCommit { message } => assert_eq!(message, "fix"),
            other => panic!("expected GitCommit, got {other:?}"),
        }
        match parse_tool_call("git_diff", r#"{"path":"src/a.rs"}"#).unwrap() {
            AgentCommand::GitDiff { path } => assert_eq!(path.as_deref(), Some("src/a.rs")),
            other => panic!("expected GitDiff, got {other:?}"),
        }
        match parse_tool_call("git_diff", r"{}").unwrap() {
            AgentCommand::GitDiff { path } => assert!(path.is_none()),
            other => panic!("expected GitDiff, got {other:?}"),
        }
    }
}
