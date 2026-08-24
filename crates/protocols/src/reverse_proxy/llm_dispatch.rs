//! LLM Gateway 分流抽象（由装配层注入，解耦 protocols → llm 依赖）。
//!
//! 反向代理的 shared listener 在 Host 匹配到 LLM Gateway 域名时，把请求
//! 交给 [`LlmDispatcher`] 处理；不命中则继续走普通反代。
//! 装配层（`llm` 模块）实现该 trait 并注入 `ReverseProxyState`。

use std::pin::Pin;
use std::sync::Arc;

use axum::{body::Body, http::Request, response::Response};

/// LLM Gateway 请求分流器。
///
/// `try_handle` 返回 `Ok(response)` 表示 host 命中 LLM Gateway 且已处理；
/// 返回 `Err(req)` 表示 host 不命中，原始请求返回给调用方继续走普通反代。
pub trait LlmDispatcher: Send + Sync {
    fn try_handle(
        self: Arc<Self>,
        host: String,
        req: Request<Body>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Response, Request<Body>>> + Send>>;
}
