use tracing_subscriber::{prelude::*, util::SubscriberInitExt, EnvFilter};

/// 使用默认 `info` 级别初始化日志。
pub fn init_logging() {
    init_logging_with_level("info");
}

/// 使用指定默认级别初始化日志（可被 `RUST_LOG` 环境变量覆盖）。
pub fn init_logging_with_level(default_level: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(filter)
        .try_init()
        .ok();
}

/// Initialize logging with an additional custom layer (e.g., LogLayer for log capture).
/// The extra_layer is registered alongside the default fmt layer and EnvFilter.
/// The extra_layer is subscribed directly to [`tracing_subscriber::Registry`] so the function bound
/// `L: Layer<Registry>` can be satisfied without knowing the concrete layered type.
pub fn init_logging_with_layer<L>(default_level: &str, extra_layer: L)
where
    L: tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static,
{
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    tracing_subscriber::registry()
        .with(extra_layer)
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(filter)
        .try_init()
        .ok();
}
