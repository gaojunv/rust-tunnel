//! tool_result 行 content 的结构化格式（M2 契约，与前端 ChatStream 严格一致）。
//!
//! 2026-08 起 ACP 路径把 content 从纯文本改写为 JSON 字符串：
//! `{"text": string, "status": string, "diffs"?: [...], "locations"?: [...]}`
//! （空字段省略）。status 落库修复「失败工具刷新后恒显 ✓」（旧实现只落 result
//! 文本、status 丢失，前端无法区分失败）；diffs/locations 落库使刷新后 diff 展示
//! 不丢（此前只有 ToolCallUpdate 实时帧携带）。
//!
//! 存量旧行是纯文本，所有读取方（session load 重放、distill/compact 渲染）必须
//! 向后兼容：JSON 解析失败 / 非对象 / 缺 string text → 走旧路径原样使用。
//!
//! 落库语义（[`tool_result_persist_content`]）：status 为 running/completed（或缺省）
//! 且 text/diffs/locations 全空的中间态是「空占位」，传 "" 让 upsert 不覆盖已落库
//! 的真实结果；其余情况（含 failed 等异常终态）落结构化 JSON——即使 text 为空，
//! 异常终态也必须落库，前端才能把卡片打叉。
//!
//! 实现已下移至 `crate::db::tool_result`（存储关注点），此处仅 re-export 保持兼容。

pub use crate::db::tool_result::*;
