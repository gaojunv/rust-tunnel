//! 控制器宿主错误类型。

use thiserror::Error;

/// 控制器宿主错误。
#[derive(Debug, Error)]
pub enum ControllerError {
    /// 配置不合法（地址缺端口、token 为空等）。
    #[error("配置无效：{0}")]
    InvalidConfig(String),

    /// 命令参数不合法（目标为空、帧率越界等）；命令**没有**下发。
    #[error("命令参数无效：{0}")]
    InvalidCommand(String),

    /// 当前线程没有 Tokio 运行时。
    ///
    /// [`ControllerHost::start`](crate::ControllerHost::start) 会派生控制通道与状态镜像
    /// 两个后台任务，必须先有运行时（Android 层应在自己的 Tokio runtime 内调用）。
    #[error("当前线程没有 Tokio 运行时；请在 Tokio 运行时内启动控制器宿主")]
    NoRuntime,

    /// 远程控制引擎错误。
    #[error("远程控制引擎错误：{0}")]
    Remote(#[from] rust_tunnel_remote::RemoteError),
}

/// 控制器宿主的结果别名。
pub type ControllerResult<T> = Result<T, ControllerError>;
