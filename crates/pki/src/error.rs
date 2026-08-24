//! ACME 子系统结构化错误类型。
//!
//! 替代原 `anyhow::Result`：`Display` 文本与原 anyhow 最外层 context 完全
//! 一致（对外 API 错误消息不变），同时保留结构化 `source` 链。

/// ACME 证书签发/存储/解析的统一错误。
#[derive(Debug, thiserror::Error)]
pub enum AcmeError {
    /// 证书存储 IO 失败（创建目录/读写 PEM 文件），`context` 为操作描述。
    #[error("{context}")]
    Storage {
        /// 操作描述（原 anyhow context 文本）。
        context: &'static str,
        /// 底层 IO 错误。
        source: std::io::Error,
    },
    /// ACME 账户凭据序列化/反序列化失败。
    #[error("{context}")]
    AccountSerde {
        /// 操作描述（原 anyhow context 文本）。
        context: &'static str,
        /// 底层 JSON 错误。
        source: serde_json::Error,
    },
    /// instant-acme 协议交互失败（下单/授权/challenge/finalize）。
    #[error("{context}")]
    Protocol {
        /// 操作描述（原 anyhow context 文本）。
        context: &'static str,
        /// 底层 ACME 协议错误。
        source: instant_acme::Error,
    },
    /// 证书 PEM/DER 解析失败。
    #[error("Failed to parse certificate: {0}")]
    ParseCertificate(String),
    /// 私钥 PEM 解析失败。
    #[error("Failed to parse private key: {0}")]
    ParsePrivateKey(String),
    /// PEM 文件中未找到私钥。
    #[error("No private key found")]
    NoPrivateKey,
    /// 不支持的私钥类型。
    #[error("Unsupported key type: {0}")]
    UnsupportedKeyType(String),
    /// TLS ServerConfig 构建失败。
    #[error("Failed to create server config: {0}")]
    ServerConfig(String),
    /// rcgen 证书参数构建失败。
    #[error("Failed to create certificate params: {0}")]
    CertParams(String),
    /// CSR 序列化失败。
    #[error("Failed to serialize CSR: {0}")]
    SerializeCsr(String),
    /// rcgen 密钥对生成失败。
    #[error("Failed to generate key pair")]
    GenerateKeyPair(#[source] rcgen::Error),
    /// DNS provider 调用失败（域名格式/API 错误/传播超时，动态文案保留原文）。
    #[error("{0}")]
    Dns(String),
    /// 其余静态/动态文案错误（challenge/order/manager 编排语义）。
    #[error("{0}")]
    Message(String),
    /// instant-acme 协议错误直传（裸 `?` 传播，display 与原 anyhow 一致）。
    #[error(transparent)]
    Acme(#[from] instant_acme::Error),
    /// reqwest HTTP 错误直传（裸 `?` 传播，display 与原 anyhow 一致）。
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// 持久化层错误直传（裸 `?` 传播，display 与原 anyhow 一致）。
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    /// 内层 [`AcmeError`] 叠加静态 context（对应 anyhow `.context()` 语义：
    /// Display 只显示最外层 context，source 链保留）。
    #[error("{context}")]
    Context {
        /// 操作描述（原 anyhow context 文本）。
        context: &'static str,
        /// 内层错误。
        source: Box<AcmeError>,
    },
}

/// ACME 子系统 Result 别名。
pub type AcmeResult<T> = Result<T, AcmeError>;

impl AcmeError {
    /// 构造 [`AcmeError::Message`]（静态文案）。
    pub fn msg(text: &'static str) -> Self {
        Self::Message(text.to_string())
    }

    /// 构造 [`AcmeError::Message`]（动态文案）。
    pub fn msgf(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }

    /// 构造 [`AcmeError::Storage`]。
    pub fn storage(context: &'static str) -> impl FnOnce(std::io::Error) -> Self {
        move |source| Self::Storage { context, source }
    }

    /// 构造 [`AcmeError::Protocol`]。
    pub fn protocol(context: &'static str) -> impl FnOnce(instant_acme::Error) -> Self {
        move |source| Self::Protocol { context, source }
    }

    /// 构造 [`AcmeError::Context`]（内层错误叠加 context）。
    pub fn wrap(context: &'static str) -> impl FnOnce(AcmeError) -> Self {
        move |source| Self::Context {
            context,
            source: Box::new(source),
        }
    }
}
