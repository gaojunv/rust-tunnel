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
    // 非 rag 构建下 remember schema 的 push 被 cfg 掉，`mut` 无消费者 → allow。
    #[allow(unused_mut)]
    let mut tools = vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run a shell command in the workspace. Returns stdout/stderr/exit_code.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cmd": {"type": "string", "description": "The shell command to run"},
                        "cwd": {"type": "string", "description": "Optional subdirectory of the workspace to run in"},
                        "timeout_secs": {"type": "integer", "description": "Command timeout in seconds (max 3600, default 120)"}
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
                "name": "git_stage",
                "description": "Stage files (git add) by relative workspace paths.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "paths": {"type": "array", "items": {"type": "string"}, "description": "Relative file/dir paths to stage"}
                    },
                    "required": ["paths"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_unstage",
                "description": "Unstage files (git restore --staged) by relative workspace paths.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "paths": {"type": "array", "items": {"type": "string"}, "description": "Relative file/dir paths to unstage"}
                    },
                    "required": ["paths"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_log",
                "description": "Show recent commit history (git log).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "Number of commits to show (default 50)"}
                    }
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_show",
                "description": "Show a commit (git show): commit metadata plus diff.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "rev": {"type": "string", "description": "Revision to show (default HEAD)"}
                    }
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_branch",
                "description": "List, create, or delete branches.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list", "create", "delete"]},
                        "name": {"type": "string", "description": "Branch name (required for create/delete)"},
                        "force": {"type": "boolean", "description": "Force-delete (git branch -D) when action=delete"}
                    },
                    "required": ["action"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_checkout",
                "description": "Switch to a branch (optionally create it first).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "branch": {"type": "string"},
                        "create": {"type": "boolean", "description": "Create the branch (git checkout -b)"}
                    },
                    "required": ["branch"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_pull",
                "description": "Pull changes from the upstream remote (git pull).",
                "parameters": {"type": "object", "properties": {}}
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_revert",
                "description": "Revert a commit by creating a new commit (git revert <rev>).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "rev": {"type": "string"}
                    },
                    "required": ["rev"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_reset",
                "description": "Reset HEAD to a revision (default HEAD). mode: soft|mixed|hard.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "rev": {"type": "string"},
                        "mode": {"type": "string", "enum": ["soft", "mixed", "hard"]}
                    }
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "git_stash",
                "description": "Stash operations: list, push (with optional message), apply, pop, or drop (by index).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list", "push", "apply", "pop", "drop"]},
                        "message": {"type": "string", "description": "Stash message (action=push)"},
                        "index": {"type": "integer", "description": "Stash index (action=apply/pop/drop)"}
                    },
                    "required": ["action"]
                }
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
    ];
    // AI 记忆体 remember 工具：服务端本地短路（runner.rs handle_tool_calls 提前
    // 拦截，不进 AgentCommand 协议）。description/parameters 与 MCP 端点共用
    // memory 模块的共享 schema，避免两处漂移。仅 rag feature 下出现在 schema——
    // 无 rag 构建模型看不到该工具，不会调用。
    #[cfg(feature = "rag")]
    {
        tools.push(serde_json::json!({
            "type": "function",
            "function": {
                "name": "remember",
                "description": crate::agent::memory::REMEMBER_TOOL_DESCRIPTION,
                "parameters": crate::agent::memory::remember_tool_schema(),
            }
        }));
        // Skill 库 use_skill 工具：服务端本地短路（同 remember，不进 AgentCommand
        // 协议、不落审批）。会话开始已注入 <skills> 清单（name + 触发边界），模型
        // 按需调本工具拉全文。
        tools.push(serde_json::json!({
            "type": "function",
            "function": {
                "name": "use_skill",
                "description": "Load the full content of a skill from the skill library by its name. A list of available skills (name + description) was provided at the start of this session — call this only for a skill in that list when you need its detailed steps.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "The skill name exactly as listed at session start"}
                    },
                    "required": ["name"]
                }
            }
        }));
    }
    tools
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

/// 取字符串数组参数（pathspec 用）；缺失或含非字符串元素报错。
fn arg_opt_usize(args: &serde_json::Value, key: &str) -> Option<usize> {
    args.get(key).and_then(|v| v.as_u64()).map(|n| n as usize)
}

fn arg_str_array(
    args: &serde_json::Value,
    key: &str,
    tool: &str,
) -> Result<Vec<String>, String> {
    let arr = args
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("tool '{tool}' requires array argument '{key}'"))?;
    arr.iter()
        .map(|v| {
            v.as_str()
                .map(ToString::to_string)
                .ok_or_else(|| format!("tool '{tool}' argument '{key}' must be strings"))
        })
        .collect()
}

/// 构造 `git <sub>... -- <paths...>` 参数：pathspec 必须用 `--` 分隔（git_plan
/// fail-closed 约束）；非空 + 总量 900KB 上限 + 单路径长度上限。
fn git_paths_cmd(tool: &str, prefix: &[&str], paths: &[String]) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Err(format!("tool '{tool}': paths must not be empty"));
    }
    let mut total = 0usize;
    for p in paths {
        check_path_len(p, "paths")?;
        total += p.len();
    }
    if total > MAX_TOOL_INPUT {
        return Err("paths too large (>900KB)".to_string());
    }
    let mut out: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
    out.push("--".to_string());
    out.extend(paths.iter().cloned());
    Ok(out)
}

/// 把待校验的 git 参数经 git_plan 规划为可执行的 GitExec 命令。非法参数在此
/// fail-closed 报错：模型在 parse 阶段即收到错误，而非隧道执行失败或注入风险。
fn plan_git_cmd(tool: &str, args: Vec<String>) -> Result<AgentCommand, String> {
    let planned = super::git_plan::plan(&args).map_err(|e| format!("tool '{tool}': {e}"))?;
    Ok(AgentCommand::GitExec { args: planned.args })
}

/// Cap on a single tool input payload. The control-channel protocol cap is 1MB
/// and is enforced on the client's read side, where an oversize frame kills the
/// connection. Reject inputs below that cap here so an oversized write/shell cmd
/// becomes a model-facing error instead of tearing down the client.
const MAX_TOOL_INPUT: usize = 900 * 1024;

/// File path maximum length (POSIX PATH_MAX is usually 4096).
const MAX_PATH_LEN: usize = 4096;

/// Git commit message maximum length (prevents an oversized message from
/// blowing past the control-channel cap). 面板 git/commit 与 git/stash/push
/// 共用同一上限。
pub(crate) const MAX_COMMIT_MSG_LEN: usize = 64 * 1024;

/// Validate a path-like argument against `MAX_PATH_LEN`.
fn check_path_len(value: &str, arg: &str) -> Result<(), String> {
    if value.len() > MAX_PATH_LEN {
        return Err(format!("{arg} too long (>{MAX_PATH_LEN} bytes)"));
    }
    Ok(())
}

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
            let cwd = arg_opt_str(&args, "cwd");
            if let Some(cwd) = &cwd {
                check_path_len(cwd, "cwd")?;
            }
            // timeout_secs：有值时构造 ShellWithTimeout，无值时保持 Shell（120s 默认）
            match args.get("timeout_secs").and_then(|v| v.as_u64()) {
                Some(secs) if secs > 0 => Ok(AgentCommand::ShellWithTimeout {
                    cmd: cmd.to_string(),
                    cwd,
                    timeout_secs: secs.clamp(1, 3600),
                }),
                _ => Ok(AgentCommand::Shell { cmd: cmd.to_string(), cwd }),
            }
        }
        "read_file" => {
            let path = arg_str(&args, "path", name)?;
            check_path_len(path, "path")?;
            Ok(AgentCommand::ReadFile {
                path: path.to_string(),
            })
        }
        "write_file" => {
            let path = arg_str(&args, "path", name)?;
            check_path_len(path, "path")?;
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
        "list_dir" => {
            let path = arg_str(&args, "path", name)?;
            check_path_len(path, "path")?;
            Ok(AgentCommand::ListDir {
                path: path.to_string(),
            })
        }
        "git_status" => Ok(AgentCommand::GitStatus),
        "git_diff" => {
            let path = arg_opt_str(&args, "path");
            if let Some(path) = &path {
                check_path_len(path, "path")?;
            }
            Ok(AgentCommand::GitDiff { path })
        }
        "git_commit" => {
            let message = arg_str(&args, "message", name)?;
            if message.len() > MAX_COMMIT_MSG_LEN {
                return Err(format!(
                    "message too long (>{MAX_COMMIT_MSG_LEN} bytes)"
                ));
            }
            Ok(AgentCommand::GitCommit {
                message: message.to_string(),
            })
        }
        "git_push" => Ok(AgentCommand::GitPush),
        "git_stage" => {
            let paths = arg_str_array(&args, "paths", name)?;
            plan_git_cmd(name, git_paths_cmd(name, &["add"], &paths)?)
        }
        "git_unstage" => {
            let paths = arg_str_array(&args, "paths", name)?;
            plan_git_cmd(name, git_paths_cmd(name, &["restore", "--staged"], &paths)?)
        }
        "git_log" => {
            let limit = arg_opt_usize(&args, "limit").unwrap_or(50).clamp(1, 500);
            plan_git_cmd(
                name,
                vec!["log".to_string(), "-n".to_string(), limit.to_string()],
            )
        }
        "git_show" => {
            let rev = arg_opt_str(&args, "rev");
            let mut git_args = vec!["show".to_string()];
            if let Some(rev) = rev {
                git_args.push(rev);
            }
            plan_git_cmd(name, git_args)
        }
        "git_branch" => {
            let action = arg_str(&args, "action", name)?;
            let branch_name = arg_opt_str(&args, "name");
            let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            let git_args = match action {
                "list" => vec!["branch".to_string()],
                "create" => {
                    let bname = branch_name
                        .ok_or_else(|| format!("tool '{name}': name required for create"))?;
                    vec!["branch".to_string(), bname]
                }
                "delete" => {
                    let bname = branch_name
                        .ok_or_else(|| format!("tool '{name}': name required for delete"))?;
                    vec![
                        "branch".to_string(),
                        if force { "-D" } else { "-d" }.to_string(),
                        bname,
                    ]
                }
                other => {
                    return Err(format!(
                        "tool '{name}': invalid action '{other}' (list|create|delete)"
                    ));
                }
            };
            plan_git_cmd(name, git_args)
        }
        "git_checkout" => {
            let branch = arg_str(&args, "branch", name)?;
            let create = args.get("create").and_then(|v| v.as_bool()).unwrap_or(false);
            let git_args = if create {
                vec!["checkout".to_string(), "-b".to_string(), branch.to_string()]
            } else {
                vec!["checkout".to_string(), branch.to_string()]
            };
            plan_git_cmd(name, git_args)
        }
        "git_pull" => plan_git_cmd(name, vec!["pull".to_string()]),
        "git_revert" => {
            let rev = arg_str(&args, "rev", name)?;
            plan_git_cmd(name, vec!["revert".to_string(), rev.to_string()])
        }
        "git_reset" => {
            let rev = arg_opt_str(&args, "rev");
            let mode = arg_opt_str(&args, "mode");
            let mut git_args = vec!["reset".to_string()];
            if let Some(m) = mode {
                if !matches!(m.as_str(), "soft" | "mixed" | "hard") {
                    return Err(format!("tool '{name}': mode must be soft|mixed|hard"));
                }
                git_args.push(format!("--{m}"));
            }
            if let Some(rev) = rev {
                git_args.push(rev);
            }
            plan_git_cmd(name, git_args)
        }
        "git_stash" => {
            let action = arg_str(&args, "action", name)?;
            let message = arg_opt_str(&args, "message");
            let index = args.get("index").and_then(|v| v.as_u64()).map(|n| n as usize);
            let git_args = match action {
                "list" => vec!["stash".to_string(), "list".to_string()],
                "push" => {
                    let mut a = vec!["stash".to_string(), "push".to_string()];
                    if let Some(m) = message {
                        if m.len() > MAX_COMMIT_MSG_LEN {
                            return Err(format!(
                                "message too long (>{MAX_COMMIT_MSG_LEN} bytes)"
                            ));
                        }
                        a.push("-m".to_string());
                        a.push(m);
                    }
                    a
                }
                "apply" | "pop" | "drop" => {
                    let idx = index.ok_or_else(|| {
                        format!("tool '{name}': index required for action '{action}'")
                    })?;
                    vec![
                        "stash".to_string(),
                        action.to_string(),
                        format!("stash@{{{idx}}}"),
                    ]
                }
                other => {
                    return Err(format!(
                        "tool '{name}': invalid action '{other}' (list|push|apply|pop|drop)"
                    ));
                }
            };
            plan_git_cmd(name, git_args)
        }
        "search" => {
            let pattern = arg_str(&args, "pattern", name)?;
            if pattern.len() > MAX_TOOL_INPUT {
                return Err("pattern too large (>900KB)".to_string());
            }
            let path = arg_str(&args, "path", name)?;
            check_path_len(path, "path")?;
            let include = arg_opt_str(&args, "include");
            if let Some(include) = &include {
                check_path_len(include, "include")?;
            }
            Ok(AgentCommand::Search {
                pattern: pattern.to_string(),
                path: path.to_string(),
                include,
            })
        }
        "patch_file" => {
            let path = arg_str(&args, "path", name)?;
            check_path_len(path, "path")?;
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

    #[test]
    fn test_schema_covers_git_exec_tools() {
        let schema = agent_tools_schema();
        let names: Vec<&str> = schema
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        for expected in [
            "git_stage",
            "git_unstage",
            "git_log",
            "git_show",
            "git_branch",
            "git_checkout",
            "git_pull",
            "git_revert",
            "git_reset",
            "git_stash",
        ] {
            assert!(names.contains(&expected), "missing tool: {expected}");
        }
    }

    #[test]
    fn test_parse_git_stage_unstage() {
        match parse_tool_call("git_stage", r#"{"paths":["a.rs","b.rs"]}"#).unwrap() {
            AgentCommand::GitExec { args } => {
                assert_eq!(args, ["add", "--", "a.rs", "b.rs"]);
            }
            other => panic!("expected GitExec, got {other:?}"),
        }
        match parse_tool_call("git_unstage", r#"{"paths":["a.rs"]}"#).unwrap() {
            AgentCommand::GitExec { args } => {
                assert_eq!(args, ["restore", "--staged", "--", "a.rs"]);
            }
            other => panic!("expected GitExec, got {other:?}"),
        }
        // 空 paths / 缺失 paths 拒绝
        assert!(parse_tool_call("git_stage", r#"{"paths":[]}"#).is_err());
        assert!(parse_tool_call("git_stage", r"{}").is_err());
        // 路径注入在 parse 阶段拦截（fail-closed）
        assert!(parse_tool_call("git_stage", r#"{"paths":["../etc/passwd"]}"#).is_err());
        assert!(parse_tool_call("git_stage", r#"{"paths":["-rf"]}"#).is_err());
    }

    #[test]
    fn test_parse_git_log_show() {
        match parse_tool_call("git_log", r#"{"limit":10}"#).unwrap() {
            AgentCommand::GitExec { args } => assert_eq!(args, ["log", "-n", "10"]),
            other => panic!("expected GitExec, got {other:?}"),
        }
        match parse_tool_call("git_log", r"{}").unwrap() {
            AgentCommand::GitExec { args } => assert_eq!(args, ["log", "-n", "50"]), // 缺省 50
            other => panic!("expected GitExec, got {other:?}"),
        }
        match parse_tool_call("git_show", r#"{"rev":"abc123"}"#).unwrap() {
            AgentCommand::GitExec { args } => assert_eq!(args, ["show", "abc123"]),
            other => panic!("expected GitExec, got {other:?}"),
        }
        match parse_tool_call("git_show", r"{}").unwrap() {
            AgentCommand::GitExec { args } => assert_eq!(args, ["show"]),
            other => panic!("expected GitExec, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_git_branch() {
        let list = parse_tool_call("git_branch", r#"{"action":"list"}"#).unwrap();
        assert!(matches!(
            list,
            AgentCommand::GitExec { args } if args == ["branch"]
        ));
        let create = parse_tool_call(
            "git_branch",
            r#"{"action":"create","name":"feature"}"#,
        )
        .unwrap();
        assert!(matches!(
            create,
            AgentCommand::GitExec { args } if args == ["branch", "feature"]
        ));
        let del = parse_tool_call("git_branch", r#"{"action":"delete","name":"feature"}"#).unwrap();
        assert!(matches!(
            del,
            AgentCommand::GitExec { args } if args == ["branch", "-d", "feature"]
        ));
        let force_del = parse_tool_call(
            "git_branch",
            r#"{"action":"delete","name":"feature","force":true}"#,
        )
        .unwrap();
        assert!(matches!(
            force_del,
            AgentCommand::GitExec { args } if args == ["branch", "-D", "feature"]
        ));
        // 非法 action / 缺 name
        assert!(parse_tool_call("git_branch", r#"{"action":"bogus"}"#).is_err());
        assert!(parse_tool_call("git_branch", r#"{"action":"create"}"#).is_err());
        assert!(parse_tool_call("git_branch", r"{}").is_err());
    }

    #[test]
    fn test_parse_git_checkout_pull() {
        let co = parse_tool_call("git_checkout", r#"{"branch":"main"}"#).unwrap();
        assert!(matches!(
            co,
            AgentCommand::GitExec { args } if args == ["checkout", "main"]
        ));
        let co_b = parse_tool_call("git_checkout", r#"{"branch":"f","create":true}"#).unwrap();
        assert!(matches!(
            co_b,
            AgentCommand::GitExec { args } if args == ["checkout", "-b", "f"]
        ));
        assert!(parse_tool_call("git_checkout", r"{}").is_err());
        assert!(matches!(
            parse_tool_call("git_pull", r"{}").unwrap(),
            AgentCommand::GitExec { args } if args == ["pull"]
        ));
    }

    #[test]
    fn test_parse_git_revert_reset() {
        let rev = parse_tool_call("git_revert", r#"{"rev":"abc123"}"#).unwrap();
        assert!(matches!(
            rev,
            AgentCommand::GitExec { args } if args == ["revert", "abc123"]
        ));
        assert!(parse_tool_call("git_revert", r"{}").is_err());

        let reset = parse_tool_call("git_reset", r#"{"rev":"HEAD~1","mode":"hard"}"#).unwrap();
        assert!(matches!(
            reset,
            AgentCommand::GitExec { args } if args == ["reset", "--hard", "HEAD~1"]
        ));
        let reset_default = parse_tool_call("git_reset", r#"{"mode":"soft"}"#).unwrap();
        assert!(matches!(
            reset_default,
            AgentCommand::GitExec { args } if args == ["reset", "--soft"]
        ));
        let reset_empty = parse_tool_call("git_reset", r"{}").unwrap();
        assert!(matches!(
            reset_empty,
            AgentCommand::GitExec { args } if args == ["reset"]
        ));
        // 非法 mode 在 parse 阶段拒绝
        assert!(parse_tool_call("git_reset", r#"{"mode":"danger"}"#).is_err());
    }

    #[test]
    fn test_parse_git_stash() {
        let list = parse_tool_call("git_stash", r#"{"action":"list"}"#).unwrap();
        assert!(matches!(
            list,
            AgentCommand::GitExec { args } if args == ["stash", "list"]
        ));
        let push = parse_tool_call("git_stash", r#"{"action":"push"}"#).unwrap();
        assert!(matches!(
            push,
            AgentCommand::GitExec { args } if args == ["stash", "push"]
        ));
        let push_msg = parse_tool_call(
            "git_stash",
            r#"{"action":"push","message":"wip"}"#,
        )
        .unwrap();
        assert!(matches!(
            push_msg,
            AgentCommand::GitExec { args } if args == ["stash", "push", "-m", "wip"]
        ));
        let apply = parse_tool_call("git_stash", r#"{"action":"apply","index":2}"#).unwrap();
        assert!(matches!(
            apply,
            AgentCommand::GitExec { args } if args == ["stash", "apply", "stash@{2}"]
        ));
        let drop = parse_tool_call("git_stash", r#"{"action":"drop","index":0}"#).unwrap();
        assert!(matches!(
            drop,
            AgentCommand::GitExec { args } if args == ["stash", "drop", "stash@{0}"]
        ));
        // 非法 action / 缺 index
        assert!(parse_tool_call("git_stash", r#"{"action":"bogus"}"#).is_err());
        assert!(parse_tool_call("git_stash", r#"{"action":"apply"}"#).is_err());
        assert!(parse_tool_call("git_stash", r"{}").is_err());
        // stash message 超 64KB 拒绝
        let big = "x".repeat(MAX_COMMIT_MSG_LEN + 1);
        let args = serde_json::json!({"action": "push", "message": big}).to_string();
        assert!(parse_tool_call("git_stash", &args)
            .unwrap_err()
            .contains("message too long"));
    }

    #[test]
    fn test_parse_rejects_oversized_path() {
        let big = "x".repeat(MAX_PATH_LEN + 1);
        // read_file 必填 path
        let args = serde_json::json!({"path": big}).to_string();
        let err = parse_tool_call("read_file", &args).unwrap_err();
        assert!(err.contains("path too long"), "err: {err}");
        // write_file
        let args =
            serde_json::json!({"path": &big, "content": "x"}).to_string();
        let err = parse_tool_call("write_file", &args).unwrap_err();
        assert!(err.contains("path too long"), "err: {err}");
        // list_dir
        let args = serde_json::json!({"path": &big}).to_string();
        let err = parse_tool_call("list_dir", &args).unwrap_err();
        assert!(err.contains("path too long"), "err: {err}");
        // git_diff（可选 path）
        let args = serde_json::json!({"path": &big}).to_string();
        let err = parse_tool_call("git_diff", &args).unwrap_err();
        assert!(err.contains("path too long"), "err: {err}");
        // search path
        let args =
            serde_json::json!({"pattern": "x", "path": &big}).to_string();
        let err = parse_tool_call("search", &args).unwrap_err();
        assert!(err.contains("path too long"), "err: {err}");
        // patch_file path
        let args = serde_json::json!({
            "path": &big,
            "old_string": "o",
            "new_string": "n"
        })
        .to_string();
        let err = parse_tool_call("patch_file", &args).unwrap_err();
        assert!(err.contains("path too long"), "err: {err}");
    }

    #[test]
    fn test_parse_rejects_oversized_cwd() {
        let big = "x".repeat(MAX_PATH_LEN + 1);
        let args = serde_json::json!({"cmd": "ls", "cwd": big}).to_string();
        let err = parse_tool_call("shell", &args).unwrap_err();
        assert!(err.contains("cwd too long"), "err: {err}");
    }

    #[test]
    fn test_parse_rejects_oversized_commit_msg() {
        let big = "x".repeat(MAX_COMMIT_MSG_LEN + 1);
        let args = serde_json::json!({"message": big}).to_string();
        let err = parse_tool_call("git_commit", &args).unwrap_err();
        assert!(err.contains("message too long"), "err: {err}");
    }

    #[test]
    fn test_parse_rejects_oversized_include() {
        let big = "x".repeat(MAX_PATH_LEN + 1);
        let args =
            serde_json::json!({"pattern": "x", "path": ".", "include": big}).to_string();
        let err = parse_tool_call("search", &args).unwrap_err();
        assert!(err.contains("include too long"), "err: {err}");
    }

    #[test]
    fn test_parse_accepts_max_boundary_path() {
        let ok = "x".repeat(MAX_PATH_LEN);
        let args = serde_json::json!({"path": ok}).to_string();
        let cmd = parse_tool_call("read_file", &args).unwrap();
        match cmd {
            AgentCommand::ReadFile { path } => assert_eq!(path.len(), MAX_PATH_LEN),
            other => panic!("expected ReadFile, got {other:?}"),
        }
        // cwd 边界
        let cwd = "y".repeat(MAX_PATH_LEN);
        let args = serde_json::json!({"cmd": "ls", "cwd": cwd}).to_string();
        match parse_tool_call("shell", &args).unwrap() {
            AgentCommand::Shell { cwd, .. } => {
                assert_eq!(cwd.as_deref().map(str::len), Some(MAX_PATH_LEN));
            }
            other => panic!("expected Shell, got {other:?}"),
        }
        // commit message 边界（恰好 64KB）
        let msg = "z".repeat(MAX_COMMIT_MSG_LEN);
        let args = serde_json::json!({"message": msg}).to_string();
        match parse_tool_call("git_commit", &args).unwrap() {
            AgentCommand::GitCommit { message } => {
                assert_eq!(message.len(), MAX_COMMIT_MSG_LEN);
            }
            other => panic!("expected GitCommit, got {other:?}"),
        }
    }
}
