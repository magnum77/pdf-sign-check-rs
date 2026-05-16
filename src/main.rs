use std::sync::Arc;
use std::time::Duration;

use axum::{Router, routing::get, routing::post};
use tokio::{net::TcpListener, signal};
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod paperless;
mod retention;
mod scan;
mod signature;
mod webhook;
mod welcome;

use config::Config;
use paperless::PaperlessClient;
use retention::cleanup_retained_files;
use scan::ScanCoordinator;
use webhook::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    config.ensure_dirs_blocking()?;

    let _log_guard = init_logging(&config);
    cleanup_log_retention(&config).await?;
    let _log_retention_task = spawn_log_retention_task(config.clone());

    let paperless = config.paperless.clone().map(PaperlessClient::new);
    let state = Arc::new(AppState {
        config: config.clone(),
        paperless,
        scan: Arc::new(ScanCoordinator::new()),
    });
    let app = Router::new()
        .route("/", get(welcome::page))
        .route("/scan", get(scan::page))
        .route("/scan/state", get(scan::state))
        .route("/scan/events", get(scan::events))
        .route("/scan/start", post(scan::start))
        .route("/scan/cancel", post(scan::cancel))
        .route(&config.webhook_path, post(webhook::handle_webhook))
        .route("/healthz", get(webhook::health))
        .with_state(state);

    let listener = TcpListener::bind(config.bind).await?;
    info!(
        bind = %config.bind,
        webhook_path = %config.webhook_path,
        debug_dir = %config.debug_dir.display(),
        log_dir = %config.log_dir.display(),
        debug_retention_count = config.debug_retention_count,
        log_retention_count = config.log_retention_count,
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

fn init_logging(config: &Config) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("pdf_sign_check_rs=info,tower_http=info"));

    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stdout));

    if config.log_retention_count == 0 {
        subscriber.init();
        None
    } else {
        let file_appender = tracing_appender::rolling::daily(&config.log_dir, "pdf-sign-check.log");
        let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
        subscriber
            .with(fmt::layer().with_writer(file_writer).with_ansi(false))
            .init();
        Some(guard)
    }
}

async fn cleanup_log_retention(config: &Config) -> anyhow::Result<()> {
    if config.log_retention_count == 0 {
        return Ok(());
    }

    let removed = cleanup_retained_files(&config.log_dir, config.log_retention_count, |path| {
        path.file_name()
            .and_then(|value| value.to_str())
            .map(|value| value.starts_with("pdf-sign-check.log"))
            .unwrap_or(false)
    })
    .await?;

    if removed > 0 {
        info!(
            retention_count = config.log_retention_count,
            removed,
            log_dir = %config.log_dir.display(),
            "old log files removed"
        );
    }

    Ok(())
}

fn spawn_log_retention_task(config: Config) -> Option<tokio::task::JoinHandle<()>> {
    if config.log_retention_count == 0 {
        return None;
    }

    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            interval.tick().await;
            if let Err(err) = cleanup_log_retention(&config).await {
                warn!(error = %err, "failed to clean up retained log files");
            }
        }
    }))
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
