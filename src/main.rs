use std::sync::Arc;

use axum::{Router, routing::get, routing::post};
use tokio::{net::TcpListener, signal};
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod paperless;
mod signature;
mod webhook;

use config::Config;
use paperless::PaperlessClient;
use webhook::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    config.ensure_dirs_blocking()?;

    let _log_guard = init_logging(&config);

    let paperless = config.paperless.clone().map(PaperlessClient::new);
    let state = Arc::new(AppState {
        config: config.clone(),
        paperless,
    });
    let app = Router::new()
        .route(&config.webhook_path, post(webhook::handle_webhook))
        .route("/healthz", get(webhook::health))
        .with_state(state);

    let listener = TcpListener::bind(config.bind).await?;
    info!(
        bind = %config.bind,
        webhook_path = %config.webhook_path,
        debug_dir = %config.debug_dir.display(),
        log_dir = %config.log_dir.display(),
        temp_dir = %config.temp_dir.display(),
        "pdf-sign-check-rs started"
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    info!("pdf-sign-check-rs stopped");
    Ok(())
}

fn init_logging(config: &Config) -> tracing_appender::non_blocking::WorkerGuard {
    let file_appender = tracing_appender::rolling::daily(&config.log_dir, "pdf-sign-check.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("pdf_sign_check_rs=info,tower_http=info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(file_writer).with_ansi(false))
        .with(fmt::layer().with_writer(std::io::stdout))
        .init();

    guard
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = signal::ctrl_c().await {
            warn!(error = %err, "failed to install Ctrl-C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => warn!(error = %err, "failed to install SIGTERM handler"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
