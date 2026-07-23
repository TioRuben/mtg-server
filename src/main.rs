use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::{process::Command, sync::Mutex};
use tracing::{error, info};

const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const INDEX_HTML: &str = include_str!("../assets/index.html");

#[derive(Debug, Parser)]
#[command(version, about = "Serve the latest MTG FCI image over Iberia")]
struct Config {
    #[arg(long, env = "HTTP_HOST", default_value = "0.0.0.0")]
    host: String,

    #[arg(long, env = "HTTP_PORT", default_value_t = 3000)]
    port: u16,

    #[arg(long, env = "EUMETSAT_CONSUMER_KEY")]
    consumer_key: Option<String>,

    #[arg(long, env = "EUMETSAT_CONSUMER_SECRET")]
    consumer_secret: Option<String>,

    #[arg(long, env = "MTG_CACHE_DIR", default_value = "cache")]
    cache_dir: PathBuf,

    #[arg(long, env = "PYTHON", default_value = "python3")]
    python: PathBuf,

    #[arg(
        long,
        env = "MTG_PROCESSOR",
        default_value = "scripts/generate_image.py"
    )]
    processor: PathBuf,
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    runtime: Arc<Mutex<RuntimeState>>,
}

#[derive(Debug, Default)]
struct RuntimeState {
    generating: bool,
    cache: Option<CacheMetadata>,
    last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CacheMetadata {
    generated_unix: u64,
    satellite_time: String,
    product_id: String,
}

#[derive(Debug, Serialize)]
struct StatusSnapshot {
    state: &'static str,
    image_available: bool,
    image_url: Option<&'static str>,
    generated_unix: Option<u64>,
    satellite_time: Option<String>,
    message: String,
}

impl RuntimeState {
    fn begin_generation(&mut self, now: u64, image_exists: bool) -> bool {
        if self.generating || self.is_fresh(now, image_exists) {
            return false;
        }

        self.generating = true;
        self.last_error = None;
        true
    }

    fn is_fresh(&self, now: u64, image_exists: bool) -> bool {
        image_exists
            && self.cache.as_ref().is_some_and(|metadata| {
                now.saturating_sub(metadata.generated_unix) < CACHE_TTL.as_secs()
            })
    }

    fn snapshot(&self, image_exists: bool) -> StatusSnapshot {
        let (state, message) = if self.generating {
            (
                "generating",
                "Downloading and composing the latest satellite observation...".to_owned(),
            )
        } else if let Some(error) = &self.last_error {
            ("error", error.clone())
        } else if image_exists {
            ("ready", "Latest observation ready".to_owned())
        } else {
            ("empty", "No image has been generated yet".to_owned())
        };

        StatusSnapshot {
            state,
            image_available: image_exists,
            image_url: image_exists.then_some("/image/latest.png"),
            generated_unix: self.cache.as_ref().map(|metadata| metadata.generated_unix),
            satellite_time: self
                .cache
                .as_ref()
                .map(|metadata| metadata.satellite_time.clone()),
            message,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mtg=info,tower_http=info".into()),
        )
        .init();

    let config = Arc::new(Config::parse());
    tokio::fs::create_dir_all(&config.cache_dir)
        .await
        .with_context(|| format!("failed to create {}", config.cache_dir.display()))?;

    let cache = load_cached_metadata(&config.cache_dir).await;
    let state = AppState {
        config: config.clone(),
        runtime: Arc::new(Mutex::new(RuntimeState {
            cache,
            ..RuntimeState::default()
        })),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/latest", post(latest))
        .route("/image/latest.png", get(image))
        .with_state(state);

    let address = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("failed to bind {address}"))?;
    info!("listening on http://{address}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server failed")
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn status(State(state): State<AppState>) -> Json<StatusSnapshot> {
    let image_exists = state.image_path().is_file();
    Json(state.runtime.lock().await.snapshot(image_exists))
}

async fn latest(State(state): State<AppState>) -> Json<StatusSnapshot> {
    let image_exists = state.image_path().is_file();
    let should_start = state
        .runtime
        .lock()
        .await
        .begin_generation(unix_now(), image_exists);

    if should_start {
        let generation_state = state.clone();
        tokio::spawn(async move {
            generation_state.generate().await;
        });
    }

    Json(state.runtime.lock().await.snapshot(image_exists))
}

async fn image(State(state): State<AppState>) -> Response {
    match tokio::fs::read(state.image_path()).await {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=60"),
            );
            response
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (StatusCode::NOT_FOUND, "image is not ready").into_response()
        }
        Err(error) => {
            error!(%error, "failed to read cached image");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to read image").into_response()
        }
    }
}

impl AppState {
    fn image_path(&self) -> PathBuf {
        self.config.cache_dir.join("latest.png")
    }

    fn metadata_path(&self) -> PathBuf {
        self.config.cache_dir.join("latest.json")
    }

    async fn generate(&self) {
        let result = self.run_generator().await;
        let mut runtime = self.runtime.lock().await;
        runtime.generating = false;

        match result {
            Ok(metadata) => {
                info!(satellite_time = %metadata.satellite_time, "image generation completed");
                runtime.cache = Some(metadata);
                runtime.last_error = None;
            }
            Err(error) => {
                error!(%error, "image generation failed");
                runtime.last_error = Some(format!("Image generation failed: {error:#}"));
            }
        }
    }

    async fn run_generator(&self) -> Result<CacheMetadata> {
        let consumer_key = self
            .config
            .consumer_key
            .as_deref()
            .context("EUMETSAT_CONSUMER_KEY or --consumer-key is required")?;
        let consumer_secret = self
            .config
            .consumer_secret
            .as_deref()
            .context("EUMETSAT_CONSUMER_SECRET or --consumer-secret is required")?;

        let output = Command::new(&self.config.python)
            .arg(&self.config.processor)
            .arg("--output")
            .arg(self.image_path())
            .arg("--metadata")
            .arg(self.metadata_path())
            .env("EUMETSAT_CONSUMER_KEY", consumer_key)
            .env("EUMETSAT_CONSUMER_SECRET", consumer_secret)
            .output()
            .await
            .with_context(|| {
                format!(
                    "failed to start processor {}",
                    self.config.processor.display()
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            anyhow::bail!("processor exited with {}: {stderr}", output.status);
        }

        let bytes = tokio::fs::read(self.metadata_path())
            .await
            .context("processor did not write cache metadata")?;
        serde_json::from_slice(&bytes).context("processor wrote invalid cache metadata")
    }
}

async fn load_cached_metadata(cache_dir: &Path) -> Option<CacheMetadata> {
    if !cache_dir.join("latest.png").is_file() {
        return None;
    }

    let bytes = tokio::fs::read(cache_dir.join("latest.json")).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_at(generated_unix: u64) -> RuntimeState {
        RuntimeState {
            cache: Some(CacheMetadata {
                generated_unix,
                satellite_time: "2026-07-23T12:00:00Z".to_owned(),
                product_id: "test-product".to_owned(),
            }),
            ..RuntimeState::default()
        }
    }

    #[test]
    fn fresh_cache_does_not_start_generation() {
        let mut state = cached_at(1_000);
        assert!(!state.begin_generation(1_599, true));
        assert!(!state.generating);
    }

    #[test]
    fn stale_cache_starts_only_one_generation() {
        let mut state = cached_at(1_000);
        assert!(state.begin_generation(1_600, true));
        assert!(!state.begin_generation(1_601, true));
        assert!(state.generating);
    }

    #[test]
    fn missing_image_ignores_fresh_metadata() {
        let mut state = cached_at(1_000);
        assert!(state.begin_generation(1_001, false));
    }
}
