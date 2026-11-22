//! 控制器宿主配置。

use std::path::PathBuf;
use std::sync::Arc;

use rust_tunnel_client::ClientConfig;
use rust_tunnel_remote::VideoDecoderFactory;

use crate::error::{ControllerError, ControllerResult};

/// 默认客户端名（服务端按名字唯一标识，多个平板必须各自覆盖）。
pub const DEFAULT_CLIENT_NAME: &str = "android-tablet";
/// 默认日志级别。
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// 无头控制器宿主的启动配置。
///
/// 只包含「连上服务端并成为控制器」所需的最少信息；画面/音频/输入由宿主在启动后
/// 另行注入（见 [`ControllerHost`](crate::ControllerHost)）。
///
/// 编解码器工厂（[`Self::h264_decoder_factory`] / [`Self::h265_decoder_factory`]）由宿主
/// 在启动前注入（Android 上是 MediaCodec 实现，见 M1b）；未注入时协商结果只有 VP9。
///
/// `PartialEq` 是**手写**的：工厂是 trait 对象，按「是否注入 + 它声明的 `CodecId`」比较
/// （同一 codec 的两个工厂视为相等），其余字段逐个比较。
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// 服务端地址（`host:port`，可带 IPv6 方括号）。
    pub server: String,
    /// 客户端 token（服务端 Web 管理里的 client_token / 接入口令）。
    pub token: String,
    /// 客户端名（服务端按名字唯一标识）；默认 [`DEFAULT_CLIENT_NAME`]。
    pub client_name: String,
    /// 是否启用 TLS。
    pub tls: bool,
    /// 跳过证书校验（TOFU/自签场景，默认开启，与桌面客户端一致）。
    pub tls_insecure: bool,
    /// TLS SNI 主机名；`None` 时从 `server` 推导。
    pub tls_server_name: Option<String>,
    /// 日志级别（`trace` / `debug` / `info` / `warn` / `error`）。
    pub log_level: String,
    /// 控制器下载远端文件的落地目录。
    ///
    /// Android 应传入应用私有目录（`Context.getCacheDir()` 一类）；默认是
    /// `std::env::temp_dir()`，在移动端未必可写。
    pub download_dir: PathBuf,
    /// 可选的 H.264 解码器工厂（控制器用）。
    ///
    /// `None` = 本端不能解 H.264，协商时不会向被控端声明 H.264 能力。Android 在 M1b 注入
    /// MediaCodec 实现后，被控端协商到 H.264 时画面就是 `DecodedFrame::nv12`（紧包 NV12）。
    pub h264_decoder_factory: Option<Arc<dyn VideoDecoderFactory>>,
    /// 可选的 H.265/HEVC 解码器工厂（控制器用）。
    ///
    /// `None` = 本端不能解 H.265。只有本字段与 [`Self::prefer_h265`] 同时成立时协商才会选 H.265。
    pub h265_decoder_factory: Option<Arc<dyn VideoDecoderFactory>>,
    /// 是否**优先选择 H.265/HEVC**（默认 `true`）。
    ///
    /// 注入了解码器工厂且对端也声明 H.265 时才生效，否则按 `H.265 → H.264 → VP9` 自动回退。
    pub prefer_h265: bool,
}

impl ControllerConfig {
    /// 以服务端地址与 token 构造默认配置。
    #[must_use]
    pub fn new(server: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            token: token.into(),
            ..Self::default()
        }
    }

    /// 覆盖客户端名。
    #[must_use]
    pub fn with_client_name(mut self, client_name: impl Into<String>) -> Self {
        self.client_name = client_name.into();
        self
    }

    /// 覆盖 TLS 开关与「跳过证书校验」开关（语义与 [`ClientConfig`] 一致）。
    #[must_use]
    pub fn with_tls(mut self, tls: bool, insecure: bool) -> Self {
        self.tls = tls;
        self.tls_insecure = insecure;
        self
    }

    /// 覆盖 TLS SNI 主机名。
    #[must_use]
    pub fn with_tls_server_name(mut self, name: impl Into<String>) -> Self {
        self.tls_server_name = Some(name.into());
        self
    }

    /// 覆盖日志级别。
    #[must_use]
    pub fn with_log_level(mut self, level: impl Into<String>) -> Self {
        self.log_level = level.into();
        self
    }

    /// 覆盖下载目录。
    #[must_use]
    pub fn with_download_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.download_dir = dir.into();
        self
    }

    /// 注入 H.264 解码器工厂（控制器用）。
    ///
    /// 宿主在 `ControllerHost::start` 之前调用；注入后引擎才会向被控端声明 H.264 能力。
    #[must_use]
    pub fn with_h264_decoder(mut self, factory: Arc<dyn VideoDecoderFactory>) -> Self {
        self.h264_decoder_factory = Some(factory);
        self
    }

    /// 注入 H.265/HEVC 解码器工厂（控制器用）。
    ///
    /// 是否真的选 H.265 还受 [`Self::prefer_h265`] 与对端能力约束。
    #[must_use]
    pub fn with_h265_decoder(mut self, factory: Arc<dyn VideoDecoderFactory>) -> Self {
        self.h265_decoder_factory = Some(factory);
        self
    }

    /// 覆盖「优先 H.265」偏好（默认 `true`）。
    #[must_use]
    pub fn with_prefer_h265(mut self, prefer: bool) -> Self {
        self.prefer_h265 = prefer;
        self
    }

    /// 校验配置。
    ///
    /// # Errors
    ///
    /// 服务端地址缺端口/端口非法、token 为空、客户端名为空、日志级别为空或下载目录为空时
    /// 返回 [`ControllerError::InvalidConfig`]。
    pub fn validate(&self) -> ControllerResult<()> {
        validate_server(&self.server)?;
        if self.token.trim().is_empty() {
            return Err(ControllerError::InvalidConfig("token 不能为空".to_owned()));
        }
        if self.client_name.trim().is_empty() {
            return Err(ControllerError::InvalidConfig(
                "client_name 不能为空（服务端按名字唯一标识）".to_owned(),
            ));
        }
        if let Some(name) = &self.tls_server_name {
            if name.trim().is_empty() {
                return Err(ControllerError::InvalidConfig(
                    "tls_server_name 给出时不能为空".to_owned(),
                ));
            }
        }
        if self.log_level.trim().is_empty() {
            return Err(ControllerError::InvalidConfig(
                "log_level 不能为空".to_owned(),
            ));
        }
        if self.download_dir.as_os_str().is_empty() {
            return Err(ControllerError::InvalidConfig(
                "download_dir 不能为空".to_owned(),
            ));
        }
        Ok(())
    }

    /// 转成 `rust-tunnel-client` 的配置（agent/PTY/mesh 一律关闭）。
    pub(crate) fn to_client_config(&self) -> ClientConfig {
        ClientConfig {
            server: self.server.clone(),
            name: Some(self.client_name.clone()),
            password: self.token.clone(),
            mesh: None,
            mesh_name: None,
            mesh_services: Vec::new(),
            tls: self.tls,
            tls_server_name: self.tls_server_name.clone(),
            tls_insecure: self.tls_insecure,
            // 控制器只发起远程会话，不接受 agent 执行请求，也不需要 PTY 回环端口。
            enable_agent: false,
            agent_pty_port: rust_tunnel_common::DEFAULT_PTY_PORT,
            log: self.log_level.clone(),
        }
    }
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            server: String::new(),
            token: String::new(),
            client_name: DEFAULT_CLIENT_NAME.to_owned(),
            tls: true,
            // 与桌面客户端默认一致：服务端默认自签证书，TOFU 模式接受它。
            tls_insecure: true,
            tls_server_name: None,
            log_level: DEFAULT_LOG_LEVEL.to_owned(),
            download_dir: std::env::temp_dir(),
            // 未注入工厂 = 只能解 VP9；H.265 偏好与桌面端一致默认开启。
            h264_decoder_factory: None,
            h265_decoder_factory: None,
            prefer_h265: true,
        }
    }
}

impl PartialEq for ControllerConfig {
    fn eq(&self, other: &Self) -> bool {
        self.server == other.server
            && self.token == other.token
            && self.client_name == other.client_name
            && self.tls == other.tls
            && self.tls_insecure == other.tls_insecure
            && self.tls_server_name == other.tls_server_name
            && self.log_level == other.log_level
            && self.download_dir == other.download_dir
            && self.prefer_h265 == other.prefer_h265
            && factory_eq(
                self.h264_decoder_factory.as_ref(),
                other.h264_decoder_factory.as_ref(),
            )
            && factory_eq(
                self.h265_decoder_factory.as_ref(),
                other.h265_decoder_factory.as_ref(),
            )
    }
}

impl Eq for ControllerConfig {}

/// 按「是否注入 + 声明的 `CodecId`」比较两个解码器工厂（不能比较实例本身）。
fn factory_eq(
    left: Option<&Arc<dyn VideoDecoderFactory>>,
    right: Option<&Arc<dyn VideoDecoderFactory>>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.codec() == right.codec(),
        _ => false,
    }
}

/// 校验 `host:port` 形式（支持 `[::1]:3000`）。
fn validate_server(server: &str) -> ControllerResult<()> {
    let server = server.trim();
    if server.is_empty() {
        return Err(ControllerError::InvalidConfig(
            "server 不能为空（应为 host:port）".to_owned(),
        ));
    }
    let (host, port) = if let Some(rest) = server.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err(ControllerError::InvalidConfig(format!(
                "server 的 IPv6 方括号未闭合：{server}"
            )));
        };
        let Some(port) = after.strip_prefix(':') else {
            return Err(ControllerError::InvalidConfig(format!(
                "server 缺少端口（应为 [host]:port）：{server}"
            )));
        };
        (host, port)
    } else {
        let Some((host, port)) = server.rsplit_once(':') else {
            return Err(ControllerError::InvalidConfig(format!(
                "server 缺少端口（应为 host:port）：{server}"
            )));
        };
        (host, port)
    };
    if host.is_empty() {
        return Err(ControllerError::InvalidConfig(format!(
            "server 的主机部分为空：{server}"
        )));
    }
    let port: u16 = port.parse().map_err(|_| {
        ControllerError::InvalidConfig(format!("server 的端口不是合法 u16：{server}"))
    })?;
    if port == 0 {
        return Err(ControllerError::InvalidConfig(format!(
            "server 的端口不能为 0：{server}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_tunnel_remote::{CodecId, RemoteError, RemoteResult, VideoDecoderApi};

    /// 只用来验证注入/比较逻辑的解码器工厂（`create` 恒失败，不会被真正创建）。
    #[derive(Debug)]
    struct StubDecoder(CodecId);

    impl VideoDecoderFactory for StubDecoder {
        fn codec(&self) -> CodecId {
            self.0
        }

        fn create(&self) -> RemoteResult<Box<dyn VideoDecoderApi>> {
            Err(RemoteError::Codec("测试桩不创建解码器".to_owned()))
        }
    }

    #[test]
    fn defaults_are_android_friendly() {
        let cfg = ControllerConfig::default();
        assert_eq!(cfg.client_name, DEFAULT_CLIENT_NAME);
        assert_eq!(cfg.log_level, DEFAULT_LOG_LEVEL);
        assert!(cfg.tls);
        assert!(cfg.tls_insecure);
        assert!(cfg.tls_server_name.is_none());
        assert!(!cfg.download_dir.as_os_str().is_empty());
        // 默认未注入任何硬解工厂，但 H.265 偏好与桌面端一致默认开启。
        assert!(cfg.h264_decoder_factory.is_none());
        assert!(cfg.h265_decoder_factory.is_none());
        assert!(cfg.prefer_h265);
    }

    #[test]
    fn builder_injects_decoder_factories() {
        let cfg = ControllerConfig::new("host:3000", "token")
            .with_h264_decoder(Arc::new(StubDecoder(CodecId::H264)))
            .with_h265_decoder(Arc::new(StubDecoder(CodecId::H265)))
            .with_prefer_h265(false);

        assert_eq!(
            cfg.h264_decoder_factory.as_ref().map(|f| f.codec()),
            Some(CodecId::H264)
        );
        assert_eq!(
            cfg.h265_decoder_factory.as_ref().map(|f| f.codec()),
            Some(CodecId::H265)
        );
        assert!(!cfg.prefer_h265);

        // 工厂按 codec 比较：同一 codec 的另一个实例视为相等，缺失与存在不相等。
        let same = ControllerConfig::new("host:3000", "token")
            .with_h264_decoder(Arc::new(StubDecoder(CodecId::H264)))
            .with_h265_decoder(Arc::new(StubDecoder(CodecId::H265)))
            .with_prefer_h265(false);
        assert_eq!(cfg, same);
        let different = ControllerConfig::new("host:3000", "token")
            .with_h264_decoder(Arc::new(StubDecoder(CodecId::H264)));
        assert_ne!(cfg, different);
    }

    #[test]
    fn builder_overrides_fields() {
        let cfg = ControllerConfig::new("example.com:3000", "token")
            .with_client_name("pad-2")
            .with_tls(false, false)
            .with_tls_server_name("example.com")
            .with_log_level("debug")
            .with_download_dir("/tmp/rt");

        assert_eq!(cfg.server, "example.com:3000");
        assert_eq!(cfg.token, "token");
        assert_eq!(cfg.client_name, "pad-2");
        assert!(!cfg.tls);
        assert!(!cfg.tls_insecure);
        assert_eq!(cfg.tls_server_name.as_deref(), Some("example.com"));
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.download_dir, PathBuf::from("/tmp/rt"));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_missing_port() {
        let cfg = ControllerConfig::new("example.com", "token");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_bad_port() {
        for server in ["host:notaport", "host:0", "host:70000"] {
            let cfg = ControllerConfig::new(server, "token");
            assert!(cfg.validate().is_err(), "{server} 应被拒绝");
        }
    }

    #[test]
    fn validate_accepts_ipv6_brackets() {
        assert!(ControllerConfig::new("[::1]:3000", "token")
            .validate()
            .is_ok());
        assert!(ControllerConfig::new("[fe80::1]:443", "token")
            .validate()
            .is_ok());
        assert!(ControllerConfig::new("[::1]3000", "token")
            .validate()
            .is_err());
        assert!(ControllerConfig::new("[]:3000", "token")
            .validate()
            .is_err());
    }

    #[test]
    fn validate_rejects_empty_token_and_name() {
        assert!(ControllerConfig::new("host:3000", " ").validate().is_err());
        assert!(ControllerConfig::new("host:3000", "token")
            .with_client_name("  ")
            .validate()
            .is_err());
    }

    #[test]
    fn validate_rejects_empty_optional_strings() {
        assert!(ControllerConfig::new("host:3000", "token")
            .with_tls_server_name("")
            .validate()
            .is_err());
        assert!(ControllerConfig::new("host:3000", "token")
            .with_log_level("")
            .validate()
            .is_err());
    }

    #[test]
    fn client_config_maps_fields_and_disables_agent() {
        let cfg = ControllerConfig::new("host:3000", "tok")
            .with_client_name("pad")
            .with_tls(false, true)
            .with_log_level("warn");
        let client = cfg.to_client_config();
        assert_eq!(client.server, "host:3000");
        assert_eq!(client.password, "tok");
        assert_eq!(client.name.as_deref(), Some("pad"));
        assert!(!client.tls);
        assert!(client.tls_insecure);
        assert!(!client.enable_agent);
        assert_eq!(client.log, "warn");
        assert!(client.mesh.is_none());
        assert!(client.mesh_services.is_empty());
    }
}
