mod config;
mod db;
mod error;
mod importer;
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

    // Expire mapping windows, datasets, endpoints and anonymous principals, and
    // delete source files past their plan's retention.
    let sweeper = db.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(config.sweep_interval_secs.max(5)));
        loop {
            interval.tick().await;
            match store::sweep(&sweeper).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("sweep purged {n} source files"),
                Err(err) => tracing::warn!("sweep failed: {err}"),
            }
        }
    });

    let app = routes::router(AppState { db });
    let listener = tokio::net::TcpListener::bind(&config.address).await?;
    tracing::info!("listening on {}", config.address);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
