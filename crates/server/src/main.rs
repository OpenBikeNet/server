use std::net::SocketAddr;
use std::process::ExitCode;

use obn_server::config::Config;
use obn_server::metrics::Metrics;
use obn_server::routes::Routes;
use obn_server::ws::{router, spawn_tickers, AppState};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("obn-server: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let config = Config::from_env()?;
    let routes = Routes::load_dir(&config.routes_dir)?;
    let metrics =
        Metrics::open(&config.db_path).map_err(|e| format!("cannot open {}: {e}", config.db_path.display()))?;
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|e| format!("cannot bind {}: {e}", config.bind))?;
    let ids: Vec<&str> = routes.ids().collect();
    tracing::info!("listening on {}, routes: {}", config.bind, ids.join(", "));
    let state = AppState::new(config, routes, metrics);
    spawn_tickers(state.clone());
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await
    .map_err(|e| format!("server error: {e}"))
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutting down");
}
