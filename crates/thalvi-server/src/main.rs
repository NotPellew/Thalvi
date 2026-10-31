//! Thalvi server entry point: load config, bind, and serve.

use thalvi_server::config::Config;
use thalvi_server::server::router;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    if let Err(error) = dotenvy::dotenv()
        && !error.not_found()
    {
        eprintln!("could not load .env: {error}");
        std::process::exit(1);
    }
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("invalid configuration: {error}");
            std::process::exit(1);
        }
    };
    let app = match router(&config) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("could not open saved data: {error}");
            std::process::exit(1);
        }
    };
    let listener = match tokio::net::TcpListener::bind(config.addr).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("could not bind {}: {error}", config.addr);
            std::process::exit(1);
        }
    };
    tracing::info!(addr = %config.addr, "thalvi listening");
    if let Err(error) = axum::serve(listener, app).await {
        eprintln!("server failed: {error}");
        std::process::exit(1);
    }
}
