mod auth;
mod config;
mod db;
mod error;
mod importer;
mod models;
mod query;
mod routes;
mod sheet;
mod store;
#[cfg(test)]
mod tests;

use std::time::Duration;

use tracing_subscriber::EnvFilter;

use crate::{config::config, routes::AppState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,surrealdb=warn")))
        .init();

    let config = config();
    std::fs::create_dir_all(&config.upload_dir)?;
    let db = db::connect(config).await.map_err(|e| format!("database: {e}"))?;
    let state = AppState::new(db, auth::AuthSettings::from_env());
    match state.auth.mode {
        auth::AuthMode::Demo => tracing::info!(
            "demo mode: guests + sign-in with [{}]",
            state.auth.providers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        ),
        auth::AuthMode::Service => tracing::info!("service mode: callers identified by {}", state.auth.user_header),
    }

    // Expire mapping windows, datasets, endpoints, guests and login sessions, and
    // delete source files past their plan's retention.
    let (sweeper, limiter) = (state.db.clone(), state.limiter.clone());
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(config.sweep_interval_secs.max(5)));
        loop {
            interval.tick().await;
            limiter.prune();
            match store::sweep(&sweeper).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("sweep purged {n} source files"),
                Err(err) => tracing::warn!("sweep failed: {err}"),
            }
        }
    });

    let app = routes::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let listener = tokio::net::TcpListener::bind(&config.address).await?;
    tracing::info!("listening on {}", config.address);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
