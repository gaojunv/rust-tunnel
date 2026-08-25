//! Tool definitions (JSON schema) and tool-call → AgentCommand conversion.
use rust_tunnel_common::AgentCommand;

/// 任务清单项（todo_write 工具参数）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    /// 任务描述。
    pub content: String,
    /// 任务状态：pending/in_progress/completed。
    pub status: String,
    /// 进行中展示文案。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

impl TodoItem {
    /// 校验 status 值是否合法。
    #[must_use]
    pub fn is_valid_status(s: &str) -> bool {
        matches!(s, "pending" | "in_progress" | "completed")
    }
}

/// Plan 模式下暴露的只读工具名集合（与 `is_readonly_command` 对齐）。
const PLAN_MODE_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "search",
    "git_status",
    "git_diff",
    "git_log",
    "git_show",
    "git_branch",
    "code_outline",
    "read_symbol",
];

/// 客户端版本是否支持 edit_file（0.8.0+）。版本缺失/非法视为不支持（保守，
/// 避免老客户端收到未知 bincode 变体断开控制连接）。
pub fn client_supports_edit_file(client_version: Option<&str>) -> bool {
    client_version
        .and_then(super::runner::parse_version)
        .is_some_and(|v| v >= (0, 8, 0))
}

/// OpenAI tools 格式的工具声明，透传给上游 LLM。
/// `mode` 为 `"plan"` 时只暴露只读工具子集 + `todo_write`（辅助出方案），
/// 写类工具对模型不可见（模型不会调用，parse 层再兜底拒绝）。
#[must_use]
pub fn agent_tools_schema(mode: &str) -> Vec<serde_json::Value> {
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
                "description": "Read a file's content from the workspace. For large files, use offset (1-based line) and limit (max lines, default server max ~2000) to read in chunks. Results include [showing lines X-Y of N] markers — use them to continue reading.",
                "parameters": {
                    "type": "object",
                    "properties": file_props(&[
                        ("offset", serde_json::json!({"type": "integer", "description": "1-based starting line (default 1)"})),
                        ("limit", serde_json::json!({"type": "integer", "description": "Max lines to return (default ~2000)"}))
                    ]),
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
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Make multiple edits to ONE file in a single call. Edits apply sequentially, each to the result of the previous. All-or-nothing: if any edit fails, the file is not modified. old_string must match EXACTLY ONCE unless replace_all=true. Prefer this over write_file for modifying existing files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Relative path within the workspace"},
                        "edits": {
                            "type": "array",
                            "minItems": 1,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_string": {"type": "string", "description": "Exact text to find"},
                                    "new_string": {"type": "string", "description": "Replacement text"},
                                    "replace_all": {"type": "boolean", "description": "Replace all occurrences (default false)"}
                                },
                                "required": ["old_string", "new_string"]
                            },
                            "description": "Non-empty list of sequential edits"
                        }
                    },
                    "required": ["path", "edits"]
                }
            }
        }),
        // todo_write：任务清单维护工具（服务端短路，不进 AgentCommand 协议）。
        // 全量替换语义：每次调用提交完整清单。执行模式和 plan 模式都可用
        // （写清单不算写操作，有助于出方案和跟踪进度）。
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "todo_write",
                "description": "Replace the full task list. Use this to track your progress and plan. Each call replaces the entire list.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "todos": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "content": {"type": "string", "description": "Task description"},
                                    "status": {"type": "string", "enum": ["pending", "in_progress", "completed"], "description": "Task status"},
                                    "activeForm": {"type": "string", "description": "Optional present-tense label shown while in progress"}
                                },
                                "required": ["content", "status"]
                            },
                            "description": "Full task list (replaces previous)"
                        }
                    },
                    "required": ["todos"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "code_outline",
                "description": "Show the structure of a code file (functions, structs, classes, etc.) with line ranges. Use this BEFORE reading a large file to understand its structure and save tokens.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Relative path of the source file"}
                    },
                    "required": ["path"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "read_symbol",
                "description": "Read the full source code of a specific named symbol (function, class, method, etc.) from a file. Use code_outline first to find symbol names, then use this to read the exact implementation.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Relative path of the source file"},
                        "name": {"type": "string", "description": "Exact symbol name to extract (e.g. function/method/struct name)"}
                    },
                    "required": ["path", "name"]
                }
            }
        }),
    ];
    // task：子 agent 委托工具（服务端短路 spawn 子循环，不进 AgentCommand 协议）。
    tools.push(serde_json::json!({
        "type": "function",
        "function": {
            "name": "task",
            "description": "Delegate an exploration/research subtask to an independent sub-agent with its own context. The sub-agent runs a full tool loop and returns only a summary. Use for investigative tasks (code exploration, searches, multi-file reading) whose intermediate output would pollute the main context.",
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "The task description for the sub-agent to execute"},
                    "agent": {"type": "string", "description": "Optional sub-agent role name (default: general). Available roles are listed in the system prompt."}
                },
                "required": ["prompt"]
            }
        }
    }));
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
                "description": crate::memory::REMEMBER_TOOL_DESCRIPTION,
                "parameters": crate::memory::remember_tool_schema(),
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
        // Wiki 检索工具：先 wiki_search 找 ref，再 wiki_read 取全文。服务端本地
        // 短路（同 remember/use_skill），只读、plan 模式放行。会话开始已注入
        // <wikis> 清单（name + summary + 页数）。
        tools.push(serde_json::json!({
            "type": "function",
            "function": {
                "name": "wiki_search",
                "description": "Search wiki knowledge-base pages by keyword (BM25 + fallback matching) across all visible wikis, or within one wiki if 'wiki' is given. Returns refs with title/summary/snippet — call wiki_read with a ref to load the full page.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Search keywords (any language; short 1-2 char terms use substring match)"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 20, "description": "Max results to return (default 5)"},
                        "wiki": {"type": "string", "description": "Optional wiki name from the <wikis> list to search only that wiki"}
                    },
                    "required": ["query"]
                }
            }
        }));
        tools.push(serde_json::json!({
            "type": "function",
            "function": {
                "name": "wiki_read",
                "description": "Load the full content of one or more wiki pages by ref. Use wiki_search first to find the right refs, then call this to read the complete pages.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "wiki": {"type": "string", "description": "The wiki name exactly as listed in the <wikis> list"},
                        "refs": {
                            "type": "array",
                            "items": {"type": "string"},
                            "minItems": 1,
                            "maxItems": 10,
                            "description": "Page refs to read (from wiki_search results), 1-10 items"
                        }
                    },
                    "required": ["wiki", "refs"]
                }
            }
        }));
    }

    // Plan 模式：裁剪为只读子集 + todo_write（辅助出方案），写类工具不暴露给模型。
    // wiki_search/wiki_read 是纯只读的知识检索，plan 模式放行。
    if mode == "plan" {
        tools.retain(|t| {
            let name = t["function"]["name"].as_str().unwrap_or("");
            PLAN_MODE_TOOLS.contains(&name)
                || name == "todo_write"
                || name == "task"
                || name == "wiki_search"
                || name == "wiki_read"
        });
    }

    tools
}

/// 按客户端版本裁剪工具列表：≥0.8.0 用 edit_file 替换 patch_file，
/// <0.8.0 保持 patch_file（edit_file 不暴露）。write_file 两档都保留。
#[must_use]
pub fn filter_tools_for_client_version(
    mut tools: Vec<serde_json::Value>,
    client_version: Option<&str>,
) -> Vec<serde_json::Value> {
    if client_supports_edit_file(client_version) {
        tools.retain(|t| t["function"]["name"].as_str() != Some("patch_file"));
    } else {
        tools.retain(|t| t["function"]["name"].as_str() != Some("edit_file"));
    }
    tools
}

/// 角色级工具过滤：在现有 `agent_tools_schema(mode)` 基础上叠加 allow 白名单
/// 与 deny 黑名单。allow 非空 → 只保留白名单内工具；deny 非空 → 剔除。
/// plan 模式裁剪由底层 `agent_tools_schema(mode)` 完成，角色过滤叠加其上。
#[must_use]
pub fn agent_tools_schema_filtered(
    mode: &str,
    allow: Option<&[String]>,
    deny: Option<&[String]>,
) -> Vec<serde_json::Value> {
    let mut tools = agent_tools_schema(mode);
    // allow 白名单语义：空数组视为不限制（与 API 校验一致）
    if let Some(allow_list) = allow {
        if !allow_list.is_empty() {
            tools.retain(|t| {
                let name = t["function"]["name"].as_str().unwrap_or("");
                allow_list.iter().any(|a| a == name)
            });
        }
    }
    if let Some(deny_list) = deny {
        if !deny_list.is_empty() {
            tools.retain(|t| {
                let name = t["function"]["name"].as_str().unwrap_or("");
                !deny_list.iter().any(|d| d == name)
            });
        }
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
    args.get(key)
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as usize)
}

fn arg_str_array(args: &serde_json::Value, key: &str, tool: &str) -> Result<Vec<String>, String> {
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
    let mut out: Vec<String> = prefix
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
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

/// Plan 模式下被禁止的工具名集合（写类工具）。模型看不到这些工具的 schema，
/// 但若模型幻觉出写工具名，parse 层通过此函数拒绝执行。
const PLAN_BLOCKED_TOOLS: &[&str] = &[
    "shell",
    "write_file",
    "patch_file",
    "edit_file",
    "git_commit",
    "git_push",
    "git_stage",
    "git_unstage",
    "git_checkout",
    "git_pull",
    "git_revert",
    "git_reset",
    "git_stash",
];

/// Plan 模式下工具调用是否被禁止（写类工具）。返回 Ok(()) 表示允许，
/// Err(msg) 表示被 plan 模式拦截。
pub fn plan_mode_guard(tool_name: &str) -> Result<(), String> {
    if PLAN_BLOCKED_TOOLS.contains(&tool_name) {
        Err("plan mode: 写操作不可用，当前为只读调研模式。用户确认方案后切换到执行模式即可使用写工具。".to_string())
    } else {
        Ok(())
    }
}

/// 解析 todo_write 工具调用参数，返回验证后的 TodoItem 列表。
/// 校验：todos 必须为数组、每项必须有 content 和合法 status。
pub fn parse_todo_write(args_json: &str) -> Result<Vec<TodoItem>, String> {
    let args: serde_json::Value = serde_json::from_str(args_json)
        .map_err(|e| format!("invalid todo_write arguments: {e}"))?;
    let todos = args
        .get("todos")
        .and_then(|v| v.as_array())
        .ok_or("tool 'todo_write' requires array argument 'todos'")?;
    let items: Vec<TodoItem> = todos
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let content = v
                .get("content")
                .and_then(|c| c.as_str())
                .ok_or_else(|| format!("todo[{i}]: missing 'content'"))?
                .to_string();
            let status = v
                .get("status")
                .and_then(|s| s.as_str())
                .ok_or_else(|| format!("todo[{i}]: missing 'status'"))?
                .to_string();
            if !TodoItem::is_valid_status(&status) {
                return Err(format!(
                    "todo[{i}]: invalid status '{status}' (must be pending|in_progress|completed)"
                ));
            }
            let active_form = v
                .get("activeForm")
                .and_then(|a| a.as_str())
                .map(str::to_string);
            Ok(TodoItem {
                content,
                status,
                active_form,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(items)
}

/// 解析 task 工具调用参数，返回 (agent_name, prompt)。
/// `agent` 缺失/空串 → None（调用方解析为 "general" 默认角色）。
/// 校验：prompt 必须存在、非空。
pub fn parse_task_args(args_json: &str) -> Result<(Option<String>, String), String> {
    let args: serde_json::Value =
        serde_json::from_str(args_json).map_err(|e| format!("invalid task arguments: {e}"))?;
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or("tool 'task' requires string argument 'prompt'")?;
    if prompt.trim().is_empty() {
        return Err("tool 'task': prompt must not be empty".to_string());
    }
    let agent = args
        .get("agent")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    Ok((agent, prompt.to_string()))
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
pub const MAX_COMMIT_MSG_LEN: usize = 64 * 1024;

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
            match args.get("timeout_secs").and_then(serde_json::Value::as_u64) {
                Some(secs) if secs > 0 => Ok(AgentCommand::ShellWithTimeout {
                    cmd: cmd.to_string(),
                    cwd,
                    timeout_secs: secs.clamp(1, 3600),
                }),
                _ => Ok(AgentCommand::Shell {
                    cmd: cmd.to_string(),
                    cwd,
                }),
            }
        }
        "read_file" => {
            let path = arg_str(&args, "path", name)?;
            check_path_len(path, "path")?;
            let offset = args.get("offset").and_then(serde_json::Value::as_u64);
            let limit = args.get("limit").and_then(serde_json::Value::as_u64);
            if offset.is_some() || limit.is_some() {
                Ok(AgentCommand::ReadFileRange {
                    path: path.to_string(),
                    offset,
                    limit,
                })
            } else {
                Ok(AgentCommand::ReadFile {
                    path: path.to_string(),
                })
            }
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
                return Err(format!("message too long (>{MAX_COMMIT_MSG_LEN} bytes)"));
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
            let force = args
                .get("force")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
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
            let create = args
                .get("create")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
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
            let index = args
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as usize);
            let git_args = match action {
                "list" => vec!["stash".to_string(), "list".to_string()],
                "push" => {
                    let mut a = vec!["stash".to_string(), "push".to_string()];
                    if let Some(m) = message {
                        if m.len() > MAX_COMMIT_MSG_LEN {
                            return Err(format!("message too long (>{MAX_COMMIT_MSG_LEN} bytes)"));
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
        "edit_file" => {
            let path = arg_str(&args, "path", name)?;
            check_path_len(path, "path")?;
            let edits_arr = args
                .get("edits")
                .and_then(|v| v.as_array())
                .ok_or_else(|| format!("tool '{name}' requires array argument 'edits'"))?;
            if edits_arr.is_empty() {
                return Err("tool 'edit_file': edits must not be empty".to_string());
            }
            if args.to_string().len() > MAX_TOOL_INPUT {
                return Err(
                    "edit_file payload too large (>900KB); use fewer/smaller edits".to_string(),
                );
            }
            let mut edits = Vec::with_capacity(edits_arr.len());
            for (i, e) in edits_arr.iter().enumerate() {
                let old_string = e
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("tool '{name}': edits[{i}] missing 'old_string'"))?;
                let new_string = e
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("tool '{name}': edits[{i}] missing 'new_string'"))?;
                if old_string.is_empty() {
                    return Err(format!(
                        "tool '{name}': edits[{i}] old_string must not be empty"
                    ));
                }
                let replace_all = e
                    .get("replace_all")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                edits.push(rust_tunnel_common::FileEdit {
                    old_string: old_string.to_string(),
                    new_string: new_string.to_string(),
                    replace_all,
                });
            }
            Ok(AgentCommand::EditFile {
                path: path.to_string(),
                edits,
                expected_hash: None,
            })
        }
        "code_outline" => {
            let path = arg_str(&args, "path", name)?;
            check_path_len(path, "path")?;
            Ok(AgentCommand::CodeOutline {
                path: path.to_string(),
            })
        }
        "read_symbol" => {
            let path = arg_str(&args, "path", name)?;
            check_path_len(path, "path")?;
            let sym_name = arg_str(&args, "name", name)?;
            if sym_name.len() > MAX_PATH_LEN {
                return Err("name too long".to_string());
            }
            Ok(AgentCommand::ReadSymbol {
                path: path.to_string(),
                name: sym_name.to_string(),
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
        let schema = agent_tools_schema("safe");
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
        let schema = agent_tools_schema("safe");
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
        let schema = agent_tools_schema("safe");
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
        let create =
            parse_tool_call("git_branch", r#"{"action":"create","name":"feature"}"#).unwrap();
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
        let push_msg =
            parse_tool_call("git_stash", r#"{"action":"push","message":"wip"}"#).unwrap();
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
        let args = serde_json::json!({"path": &big, "content": "x"}).to_string();
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
        let args = serde_json::json!({"pattern": "x", "path": &big}).to_string();
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
        let args = serde_json::json!({"pattern": "x", "path": ".", "include": big}).to_string();
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

    #[test]
    fn test_plan_mode_schema_only_readonly() {
        let schema = agent_tools_schema("plan");
        let names: Vec<&str> = schema
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        // plan 模式只暴露只读工具 + todo_write
        for expected in [
            "read_file",
            "list_dir",
            "search",
            "git_status",
            "git_diff",
            "git_log",
            "git_show",
            "git_branch",
            "todo_write",
        ] {
            assert!(
                names.contains(&expected),
                "plan mode missing tool: {expected}"
            );
        }
        // plan 模式不暴露写工具
        for blocked in [
            "shell",
            "write_file",
            "patch_file",
            "git_commit",
            "git_push",
            "git_stage",
            "git_unstage",
            "git_checkout",
            "git_pull",
            "git_revert",
            "git_reset",
            "git_stash",
        ] {
            assert!(
                !names.contains(&blocked),
                "plan mode should not expose: {blocked}"
            );
        }
    }

    #[test]
    fn test_parse_read_file_with_range() {
        let cmd = parse_tool_call(
            "read_file",
            r#"{"path":"src/main.rs","offset":10,"limit":50}"#,
        )
        .unwrap();
        match cmd {
            AgentCommand::ReadFileRange {
                path,
                offset,
                limit,
            } => {
                assert_eq!(path, "src/main.rs");
                assert_eq!(offset, Some(10));
                assert_eq!(limit, Some(50));
            }
            other => panic!("expected ReadFileRange, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_mode_guard_blocks_write_tools() {
        assert!(plan_mode_guard("shell").is_err());
        assert!(plan_mode_guard("write_file").is_err());
        assert!(plan_mode_guard("patch_file").is_err());
        assert!(plan_mode_guard("git_commit").is_err());
        assert!(plan_mode_guard("git_push").is_err());
        assert!(plan_mode_guard("git_stage").is_err());
        assert!(plan_mode_guard("git_checkout").is_err());
        // 只读工具不受限
        assert!(plan_mode_guard("read_file").is_ok());
        assert!(plan_mode_guard("list_dir").is_ok());
        assert!(plan_mode_guard("search").is_ok());
        assert!(plan_mode_guard("git_status").is_ok());
        assert!(plan_mode_guard("git_diff").is_ok());
        assert!(plan_mode_guard("git_log").is_ok());
    }

    #[test]
    fn test_todo_write_schema_in_all_modes() {
        // todo_write 在执行模式和 plan 模式下都可用
        for mode in ["safe", "auto_write", "full_auto", "plan"] {
            let schema = agent_tools_schema(mode);
            let names: Vec<&str> = schema
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap())
                .collect();
            assert!(
                names.contains(&"todo_write"),
                "todo_write missing in mode: {mode}"
            );
        }
    }

    #[test]
    fn test_parse_todo_write_valid() {
        let args = r#"{"todos":[{"content":"Build feature X","status":"in_progress","activeForm":"Building feature X"},{"content":"Write tests","status":"pending"}]}"#;
        let items = parse_todo_write(args).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].content, "Build feature X");
        assert_eq!(items[0].status, "in_progress");
        assert_eq!(items[0].active_form.as_deref(), Some("Building feature X"));
        assert_eq!(items[1].content, "Write tests");
        assert_eq!(items[1].status, "pending");
        assert!(items[1].active_form.is_none());
    }

    #[test]
    fn test_parse_todo_write_empty_list() {
        let items = parse_todo_write(r#"{"todos":[]}"#).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn test_parse_todo_write_invalid_status() {
        let err =
            parse_todo_write(r#"{"todos":[{"content":"x","status":"invalid"}]}"#).unwrap_err();
        assert!(err.contains("invalid status"));
    }

    #[test]
    fn test_parse_todo_write_missing_content() {
        let err = parse_todo_write(r#"{"todos":[{"status":"pending"}]}"#).unwrap_err();
        assert!(err.contains("missing 'content'"));
    }

    #[test]
    fn test_parse_todo_write_missing_status() {
        let err = parse_todo_write(r#"{"todos":[{"content":"x"}]}"#).unwrap_err();
        assert!(err.contains("missing 'status'"));
    }

    #[test]
    fn test_parse_todo_write_missing_todos() {
        let err = parse_todo_write(r"{}").unwrap_err();
        assert!(err.contains("requires array argument 'todos'"));
    }

    #[test]
    fn test_parse_todo_write_invalid_json() {
        let err = parse_todo_write("not json").unwrap_err();
        assert!(err.contains("invalid todo_write arguments"));
    }

    #[test]
    fn test_parse_todo_write_all_statuses() {
        let args = r#"{"todos":[{"content":"a","status":"pending"},{"content":"b","status":"in_progress"},{"content":"c","status":"completed"}]}"#;
        let items = parse_todo_write(args).unwrap();
        assert_eq!(items[0].status, "pending");
        assert_eq!(items[1].status, "in_progress");
        assert_eq!(items[2].status, "completed");
    }

    #[test]
    fn test_todo_item_valid_status() {
        assert!(TodoItem::is_valid_status("pending"));
        assert!(TodoItem::is_valid_status("in_progress"));
        assert!(TodoItem::is_valid_status("completed"));
        assert!(!TodoItem::is_valid_status("done"));
        assert!(!TodoItem::is_valid_status(""));
    }

    // ── task 工具 ──────────────────────────────────────────────

    #[test]
    fn test_task_schema_in_safe_mode() {
        let schema = agent_tools_schema("safe");
        let names: Vec<&str> = schema
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"task"), "task missing in safe mode");
    }

    #[test]
    fn test_task_schema_in_plan_mode() {
        let schema = agent_tools_schema("plan");
        let names: Vec<&str> = schema
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"task"), "task missing in plan mode");
    }

    #[test]
    fn test_parse_task_args_valid() {
        let (agent, prompt) = parse_task_args(r#"{"prompt":"find all callers of foo"}"#).unwrap();
        assert!(agent.is_none());
        assert_eq!(prompt, "find all callers of foo");
    }

    #[test]
    fn test_parse_task_args_with_agent() {
        let (agent, prompt) =
            parse_task_args(r#"{"prompt":"review code","agent":"explore"}"#).unwrap();
        assert_eq!(agent.as_deref(), Some("explore"));
        assert_eq!(prompt, "review code");
    }

    #[test]
    fn test_parse_task_args_empty_agent_is_none() {
        let (agent, _) = parse_task_args(r#"{"prompt":"hi","agent":""}"#).unwrap();
        assert!(agent.is_none(), "空 agent 应解析为 None");
    }

    #[test]
    fn test_parse_task_args_whitespace_agent_is_none() {
        let (agent, _) = parse_task_args(r#"{"prompt":"hi","agent":"  "}"#).unwrap();
        assert!(agent.is_none(), "空白 agent 应解析为 None");
    }

    #[test]
    fn test_parse_task_args_empty_prompt() {
        let err = parse_task_args(r#"{"prompt":""}"#).unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn test_parse_task_args_whitespace_prompt() {
        let err = parse_task_args(r#"{"prompt":"   "}"#).unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn test_parse_task_args_missing_prompt() {
        let err = parse_task_args(r"{}").unwrap_err();
        assert!(err.contains("requires string argument 'prompt'"));
    }

    #[test]
    fn test_parse_task_args_invalid_json() {
        let err = parse_task_args("not json").unwrap_err();
        assert!(err.contains("invalid task arguments"));
    }

    #[test]
    fn test_parse_task_args_prompt_not_string() {
        let err = parse_task_args(r#"{"prompt":123}"#).unwrap_err();
        assert!(err.contains("requires string argument 'prompt'"));
    }

    #[test]
    fn test_parse_read_file_without_range() {
        let cmd = parse_tool_call("read_file", r#"{"path":"src/main.rs"}"#).unwrap();
        assert!(matches!(cmd, AgentCommand::ReadFile { .. }));
    }

    #[test]
    fn test_parse_code_outline() {
        match parse_tool_call("code_outline", r#"{"path":"src/main.rs"}"#).unwrap() {
            AgentCommand::CodeOutline { path } => assert_eq!(path, "src/main.rs"),
            other => panic!("expected CodeOutline, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_read_symbol() {
        match parse_tool_call("read_symbol", r#"{"path":"src/main.rs","name":"main"}"#).unwrap() {
            AgentCommand::ReadSymbol { path, name } => {
                assert_eq!(path, "src/main.rs");
                assert_eq!(name, "main");
            }
            other => panic!("expected ReadSymbol, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_read_symbol_requires_name() {
        assert!(parse_tool_call("read_symbol", r#"{"path":"a.rs"}"#).is_err());
    }

    // ── edit_file 工具 ──────────────────────────────────────

    #[test]
    fn test_parse_edit_file_valid() {
        let args = r#"{"path":"src/a.rs","edits":[{"old_string":"fn old","new_string":"fn new"}]}"#;
        match parse_tool_call("edit_file", args).unwrap() {
            AgentCommand::EditFile {
                path,
                edits,
                expected_hash,
            } => {
                assert_eq!(path, "src/a.rs");
                assert_eq!(edits.len(), 1);
                assert_eq!(edits[0].old_string, "fn old");
                assert_eq!(edits[0].new_string, "fn new");
                assert!(!edits[0].replace_all);
                assert!(expected_hash.is_none());
            }
            other => panic!("expected EditFile, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_edit_file_multiple_edits() {
        let args = r#"{"path":"x.rs","edits":[{"old_string":"a","new_string":"b"},{"old_string":"c","new_string":"d","replace_all":true}]}"#;
        match parse_tool_call("edit_file", args).unwrap() {
            AgentCommand::EditFile { edits, .. } => {
                assert_eq!(edits.len(), 2);
                assert!(!edits[0].replace_all);
                assert!(edits[1].replace_all);
            }
            other => panic!("expected EditFile, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_edit_file_empty_edits_rejected() {
        let args = r#"{"path":"a.rs","edits":[]}"#;
        assert!(parse_tool_call("edit_file", args)
            .unwrap_err()
            .contains("must not be empty"));
    }

    #[test]
    fn test_parse_edit_file_empty_old_string_rejected() {
        let args = r#"{"path":"a.rs","edits":[{"old_string":"","new_string":"n"}]}"#;
        assert!(parse_tool_call("edit_file", args)
            .unwrap_err()
            .contains("old_string must not be empty"));
    }

    #[test]
    fn test_parse_edit_file_oversized_rejected() {
        let big = "x".repeat(901 * 1024);
        let args = serde_json::json!({"path":"a.rs","edits":[{"old_string":big,"new_string":"n"}]})
            .to_string();
        assert!(parse_tool_call("edit_file", &args)
            .unwrap_err()
            .contains("too large"));
    }

    #[test]
    fn test_parse_edit_file_missing_path() {
        assert!(parse_tool_call(
            "edit_file",
            r#"{"edits":[{"old_string":"a","new_string":"b"}]}"#
        )
        .is_err());
    }

    #[test]
    fn test_parse_edit_file_missing_edits() {
        assert!(parse_tool_call("edit_file", r#"{"path":"a.rs"}"#).is_err());
    }

    #[test]
    fn test_edit_file_schema_in_safe_mode() {
        let schema = agent_tools_schema("safe");
        let names: Vec<&str> = schema
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"edit_file"),
            "edit_file missing in safe mode"
        );
        assert!(
            names.contains(&"patch_file"),
            "patch_file still present in base schema"
        );
    }

    #[test]
    fn test_filter_tools_version_ge_080() {
        let tools = agent_tools_schema("safe");
        let filtered = filter_tools_for_client_version(tools, Some("0.8.0"));
        let names: Vec<&str> = filtered
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"edit_file"),
            "edit_file should be present for 0.8.0+"
        );
        assert!(
            !names.contains(&"patch_file"),
            "patch_file should be removed for 0.8.0+"
        );
        assert!(names.contains(&"write_file"), "write_file always present");
    }

    #[test]
    fn test_filter_tools_version_lt_080() {
        let tools = agent_tools_schema("safe");
        let filtered = filter_tools_for_client_version(tools, Some("0.7.0"));
        let names: Vec<&str> = filtered
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(
            !names.contains(&"edit_file"),
            "edit_file should not be present for <0.8.0"
        );
        assert!(
            names.contains(&"patch_file"),
            "patch_file should be present for <0.8.0"
        );
    }

    #[test]
    fn test_filter_tools_version_none() {
        let tools = agent_tools_schema("safe");
        let filtered = filter_tools_for_client_version(tools, None);
        let names: Vec<&str> = filtered
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(
            !names.contains(&"edit_file"),
            "edit_file should not be present when version unknown"
        );
        assert!(
            names.contains(&"patch_file"),
            "patch_file should be present when version unknown"
        );
    }

    #[test]
    fn test_plan_mode_guard_blocks_edit_file() {
        assert!(plan_mode_guard("edit_file").is_err());
        assert!(plan_mode_guard("write_file").is_err());
        assert!(plan_mode_guard("patch_file").is_err());
        assert!(plan_mode_guard("read_file").is_ok());
    }

    // ── agent_tools_schema_filtered ────────────────────────────

    fn tool_names(tools: &[serde_json::Value]) -> Vec<&str> {
        tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn test_filtered_no_filters_same_as_base() {
        let base = agent_tools_schema("safe");
        let filtered = agent_tools_schema_filtered("safe", None, None);
        assert_eq!(tool_names(&base), tool_names(&filtered));
    }

    #[test]
    fn test_filtered_allow_whitelist() {
        let allow = vec!["read_file".to_string(), "search".to_string()];
        let filtered = agent_tools_schema_filtered("safe", Some(&allow), None);
        let names = tool_names(&filtered);
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"search"));
        assert!(!names.contains(&"shell"));
        assert!(!names.contains(&"write_file"));
    }

    #[test]
    fn test_filtered_deny_blacklist() {
        let deny = vec!["shell".to_string(), "write_file".to_string()];
        let filtered = agent_tools_schema_filtered("safe", None, Some(&deny));
        let names = tool_names(&filtered);
        assert!(!names.contains(&"shell"));
        assert!(!names.contains(&"write_file"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"search"));
    }

    #[test]
    fn test_filtered_allow_then_deny_overlay() {
        // allow 只留 3 个，deny 再剔除 1 个
        let allow = vec![
            "read_file".to_string(),
            "shell".to_string(),
            "search".to_string(),
        ];
        let deny = vec!["shell".to_string()];
        let filtered = agent_tools_schema_filtered("safe", Some(&allow), Some(&deny));
        let names = tool_names(&filtered);
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"search"));
        assert!(!names.contains(&"shell"));
    }

    #[test]
    fn test_filtered_with_plan_mode_intersects() {
        // plan 模式已裁剪为只读子集 + todo_write + task；再叠加 allow 白名单
        let allow = vec!["read_file".to_string(), "search".to_string()];
        let filtered = agent_tools_schema_filtered("plan", Some(&allow), None);
        let names = tool_names(&filtered);
        // 白名单里的 plan-allowed 工具应保留
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"search"));
        // 白名单不含的 plan 工具应被剔除
        assert!(!names.contains(&"list_dir"));
        assert!(!names.contains(&"todo_write"));
        // 写工具本就不在 plan 模式
        assert!(!names.contains(&"shell"));
    }

    #[test]
    fn test_filtered_empty_allow_no_restriction() {
        // 空 allow 数组 = 不限制（与 API 校验语义一致）
        let allow: Vec<String> = vec![];
        let base = agent_tools_schema("safe");
        let filtered = agent_tools_schema_filtered("safe", Some(&allow), None);
        assert_eq!(tool_names(&base), tool_names(&filtered));
    }

    #[test]
    fn test_filtered_empty_deny_no_removal() {
        let deny: Vec<String> = vec![];
        let base = agent_tools_schema("safe");
        let filtered = agent_tools_schema_filtered("safe", None, Some(&deny));
        assert_eq!(tool_names(&base), tool_names(&filtered));
    }
}
