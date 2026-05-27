use tracing_subscriber::{prelude::*, util::SubscriberInitExt, EnvFilter};

pub fn init_logging() {
    init_logging_with_level("info");
}

pub fn init_logging_with_level(default_level: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(filter)
        .init();
}
