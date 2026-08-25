//! 自研 agent 回合循环：LLM 调用 → 隧道工具执行 → 结果回灌（未配置 agent_type
//! 的 workspace 的回退运行时路径；ACP 为主路径）。
//!
//! 子模块：[`prompt`] 用户消息组装、[`parse`] LLM 响应解析、[`version_gate`]
//! 客户端版本门控、[`tool_result_text`] 工具结果文本化、[`blocks`] 系统提示词
//! 块操作、[`tool_calls`] 工具调用处理、[`turn`] 回合主循环、[`exec_group`] /
//! [`subagent`] 只读并发组与子代理。

use std::sync::Arc;
use tokio::sync::mpsc;

use super::{compact, executor, roles, session::SessionRuntime, sse, tools, AgentState};
use crate::llm::{ChatCompletionRequest, ChatMessage, LlmState};
use rust_tunnel_common::{AgentCommand, AgentResult};

mod blocks;
pub mod exec_group;
mod parse;
mod prompt;
pub mod subagent;
mod tool_calls;
mod tool_result_text;
mod turn;
mod version_gate;

pub(crate) use blocks::{insert_block_before, remove_tagged_block, with_parent, ROLE_BLOCK_TAG};
pub(crate) use parse::{is_sse_line, is_sse_response, LineBuf};
pub use parse::{parse_llm_turn, LlmTurn, ParsedToolCall};
pub use prompt::{compose_user_message, MAX_REFS, MAX_REFS_TOTAL_BYTES, MAX_REF_FILE_BYTES};
pub(crate) use tool_calls::{handle_single_tool_call, handle_tool_calls, send_tool_call_delta};
pub(crate) use tool_result_text::agent_result_to_text;
pub(crate) use turn::{persist_message, record_tool_result, PersistMessageOpts};
pub use turn::{run_agent_turn, runner_persist_summary, runner_usage_ctx};
pub use version_gate::{
    client_supports_cancel, client_supports_edit, client_supports_git_exec,
    client_supports_read_range, client_supports_shell_timeout, client_supports_terminal,
    parse_version,
};
pub(crate) use version_gate::{
    client_supports_search_patch, MIN_EDIT_CLIENT_VERSION, MIN_GIT_EXEC_CLIENT_VERSION,
    MIN_READ_RANGE_CLIENT_VERSION, MIN_SEARCH_PATCH_CLIENT_VERSION,
};

/// 只读工具并发执行上限。
const READONLY_CONCURRENCY: usize = 4;

/// 子 agent 最大回合数。
const MAX_SUBAGENT_ROUNDS: usize = 15;
/// 子 agent 摘要最大字符数。
const TASK_SUMMARY_MAX_CHARS: usize = 4096;

/// 子 agent future 类型（join_all 并发 poll，需 Send 以满足 WS handler 约束）。
type SubagentFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>;
