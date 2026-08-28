use serde::Deserialize;

/// 从 TOML 配置文件解析的服务端配置（全部字段可选，未设置时回退默认值）。
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ServerConfigFile {
    /// 控制通道监听地址。
    pub control_addr: Option<String>,
    /// API 服务监听地址。
    pub api_addr: Option<String>,
    /// 管理后台登录密码。
    pub admin_password: Option<String>,
    /// JWT 签名密钥。
    pub jwt_secret: Option<String>,
    /// 客户端接入鉴权 Token。
    pub client_auth_token: Option<String>,
    /// 是否启用控制通道 TLS。
    pub tls: Option<bool>,
    /// TLS 证书文件路径（PEM）。
    pub tls_cert: Option<String>,
    /// TLS 私钥文件路径（PEM）。
    pub tls_key: Option<String>,
    /// 日志级别。
    pub log: Option<String>,
    /// SQLite 数据库路径。
    pub db_path: Option<String>,
    /// 客户端二进制归档目录。
    pub client_dist_dir: Option<String>,
    /// wiki 桌面端安装包归档目录。
    pub wiki_dist_dir: Option<String>,
    /// 是否启用 Shadowsocks。
    pub ss_enabled: Option<bool>,
    /// Shadowsocks 监听端口。
    pub ss_port: Option<u16>,
    /// Shadowsocks 加密算法。
    pub ss_cipher: Option<String>,
    /// Shadowsocks 密码。
    pub ss_password: Option<String>,
    /// 是否启用 Trojan。
    pub trojan_enabled: Option<bool>,
    /// Trojan 监听端口。
    pub trojan_port: Option<u16>,
    /// Trojan 密码。
    pub trojan_password: Option<String>,
    /// Trojan 认证失败回退地址。
    pub trojan_fallback: Option<String>,
    /// 是否启用内置 DNS 服务。
    pub dns_enabled: Option<bool>,
    /// DNS 监听地址。
    pub dns_bind: Option<String>,
    /// Tunnel 域后缀。
    pub dns_tunnel_domain: Option<String>,
    /// Mesh 域后缀。
    pub dns_mesh_domain: Option<String>,
    /// 是否启用反向代理。
    pub reverse_proxy_enabled: Option<bool>,
    /// 反向代理最大连接数。
    pub reverse_proxy_max_connections: Option<u32>,
    /// 反向代理连接超时（秒）。
    pub reverse_proxy_connection_timeout: Option<u64>,
    /// 反向代理缓冲区大小（字节）。
    pub reverse_proxy_buffer_size: Option<usize>,
    /// 是否启用 API TLS。
    pub api_tls: Option<bool>,
    /// API TLS 域名。
    pub api_domain: Option<String>,
    /// 是否启用 ACME 自动证书。
    pub acme_enabled: Option<bool>,
    /// ACME 目录地址。
    pub acme_server_url: Option<String>,
    /// ACME 证书存储目录。
    pub acme_cert_dir: Option<String>,
    /// 是否自动续期。
    pub acme_auto_renew: Option<bool>,
    /// 续期检查间隔（小时）。
    pub acme_renewal_check_interval: Option<u64>,
    /// 到期前多少天触发续期。
    pub acme_renewal_days_before_expiry: Option<u64>,
    /// ACME 注册邮箱。
    pub acme_email: Option<String>,
    /// 是否已同意 ACME 服务条款。
    pub acme_tos_agreed: Option<bool>,
}
