use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::Mutex,
    time::MissedTickBehavior,
};
use tracing::{error, info};

const SYNC_DEBOUNCE: Duration = Duration::from_secs(60);
const SYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);
const INDEX_HTML: &str = include_str!("../assets/index.html");

#[derive(Debug, Parser)]
#[command(version, about = "Serve MTG archive images over Iberia")]
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
    syncing: bool,
    cache: Option<FrameMetadata>,
    last_error: Option<String>,
    last_sync_started_unix: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct FrameMetadata {
    id: String,
    generated_unix: u64,
    satellite_time: String,
    product_id: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct TimelineManifest {
    frames: Vec<FrameMetadata>,
}

#[derive(Debug, Serialize)]
struct StatusSnapshot {
    state: &'static str,
    image_available: bool,
    image_url: Option<&'static str>,
    frame_id: Option<String>,
    generated_unix: Option<u64>,
    satellite_time: Option<String>,
    message: String,
}

#[derive(Debug, Serialize)]
struct TimelineFrameSnapshot {
    id: String,
    generated_unix: u64,
    satellite_time: String,
    image_url: String,
}

#[derive(Debug, Serialize)]
struct TimelineSnapshot {
    frames: Vec<TimelineFrameSnapshot>,
}

impl RuntimeState {
    fn begin_sync(&mut self, now: u64, force: bool) -> bool {
        if self.syncing {
            return false;
        }

        if !force
            && self
                .last_sync_started_unix
                .is_some_and(|last| now.saturating_sub(last) < SYNC_DEBOUNCE.as_secs())
        {
            return false;
        }

        self.syncing = true;
        self.last_sync_started_unix = Some(now);
        self.last_error = None;
        true
    }

    fn snapshot(&self, image_exists: bool) -> StatusSnapshot {
        let (state, message) = if self.syncing {
            (
                "syncing",
                "Synchronizing the visible-image archive…".to_owned(),
            )
        } else if let Some(error) = &self.last_error {
            ("error", error.clone())
        } else if image_exists {
            ("ready", "Latest observation ready".to_owned())
        } else {
            ("empty", "No visible image is currently cached".to_owned())
        };

        StatusSnapshot {
            state,
            image_available: image_exists,
            image_url: image_exists.then_some("/image/latest.png"),
            frame_id: self.cache.as_ref().map(|metadata| metadata.id.clone()),
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
    tokio::fs::create_dir_all(config.cache_dir.join("archive"))
        .await
        .with_context(|| {
            format!(
                "failed to create {}",
                config.cache_dir.join("archive").display()
            )
        })?;

    let cache = load_cached_metadata(&config.cache_dir).await;
    if let Some(ref metadata) = cache {
        info!(
            frame_id = %metadata.id,
            satellite_time = %metadata.satellite_time,
            "loaded cached latest image on startup"
        );
    } else {
        info!("no cached latest image found on startup");
    }

    let state = AppState {
        config: config.clone(),
        runtime: Arc::new(Mutex::new(RuntimeState {
            cache,
            ..RuntimeState::default()
        })),
    };

    info!("triggering initial archive synchronization");
    spawn_sync(&state, true).await;
    spawn_periodic_sync(state.clone());

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/latest", post(latest))
        .route("/api/timeline", get(timeline))
        .route("/image/latest.png", get(image))
        .route("/image/frames/{frame_id}", get(archived_image))
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

fn spawn_periodic_sync(state: AppState) {
    tokio::spawn(async move {
        let interval_duration = SYNC_INTERVAL;
        info!(
            interval_secs = interval_duration.as_secs(),
            "started background auto-download scheduler; next check in {} seconds ({}m)",
            interval_duration.as_secs(),
            interval_duration.as_secs() / 60
        );
        let mut interval = tokio::time::interval(interval_duration);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        interval.tick().await;

        loop {
            interval.tick().await;
            info!(
                interval_secs = interval_duration.as_secs(),
                "periodic auto-download timer triggered; starting scheduled sync check"
            );
            spawn_sync(&state, false).await;
            info!(
                next_check_in_secs = interval_duration.as_secs(),
                "next scheduled image sync check in {} seconds ({}m)",
                interval_duration.as_secs(),
                interval_duration.as_secs() / 60
            );
        }
    });
}

async fn spawn_sync(state: &AppState, force: bool) {
    let now = unix_now();
    let should_start = state.runtime.lock().await.begin_sync(now, force);

    if should_start {
        info!(force, "starting archive synchronization");
        let generation_state = state.clone();
        tokio::spawn(async move {
            generation_state.sync_archive().await;
        });
    } else {
        let runtime = state.runtime.lock().await;
        if runtime.syncing {
            info!("archive sync is already running; skipping trigger");
        } else if let Some(last) = runtime.last_sync_started_unix {
            let elapsed = now.saturating_sub(last);
            let debounce = SYNC_DEBOUNCE.as_secs();
            if elapsed < debounce {
                let remaining = debounce.saturating_sub(elapsed);
                info!(
                    elapsed_secs = elapsed,
                    debounce_secs = debounce,
                    remaining_secs = remaining,
                    "sync skipped due to debounce (last sync started {}s ago; {}s remaining until next allowed sync)",
                    elapsed,
                    remaining
                );
            }
        }
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn status(State(state): State<AppState>) -> Json<StatusSnapshot> {
    let image_exists = state.image_path().is_file();
    Json(state.runtime.lock().await.snapshot(image_exists))
}

async fn latest(State(state): State<AppState>) -> Json<StatusSnapshot> {
    info!("manual sync requested via POST /api/latest");
    spawn_sync(&state, true).await;
    let image_exists = state.image_path().is_file();
    Json(state.runtime.lock().await.snapshot(image_exists))
}

async fn timeline(State(state): State<AppState>) -> Json<TimelineSnapshot> {
    Json(TimelineSnapshot {
        frames: state.timeline_snapshot().await,
    })
}

async fn image(State(state): State<AppState>) -> Response {
    match tokio::fs::read(state.image_path()).await {
        Ok(bytes) => png_response(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (StatusCode::NOT_FOUND, "image is not ready").into_response()
        }
        Err(error) => {
            error!(%error, "failed to read cached image");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to read image").into_response()
        }
    }
}

async fn archived_image(
    State(state): State<AppState>,
    AxumPath(frame_id): AxumPath<String>,
) -> Response {
    if !is_valid_frame_id(&frame_id) {
        return (StatusCode::NOT_FOUND, "unknown frame").into_response();
    }

    let manifest = load_timeline_manifest(state.manifest_path())
        .await
        .unwrap_or_default();
    let Some(frame) = manifest
        .frames
        .into_iter()
        .find(|frame| frame.id == frame_id)
    else {
        return (StatusCode::NOT_FOUND, "unknown frame").into_response();
    };

    match state.read_archive_frame(&frame.id).await {
        Ok(bytes) => png_response(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (StatusCode::NOT_FOUND, "image is not ready").into_response()
        }
        Err(error) => {
            error!(%error, "failed to read cached image");
            (StatusCode::INTERNAL_SERVER_ERROR, "failed to read image").into_response()
        }
    }
}

fn png_response(bytes: Vec<u8>) -> Response {
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

impl AppState {
    fn image_path(&self) -> PathBuf {
        self.config.cache_dir.join("latest.png")
    }

    fn metadata_path(&self) -> PathBuf {
        self.config.cache_dir.join("latest.json")
    }

    fn manifest_path(&self) -> PathBuf {
        self.config.cache_dir.join("manifest.json")
    }

    fn archive_dir(&self) -> PathBuf {
        self.config.cache_dir.join("archive")
    }

    fn archive_path(&self, frame_id: &str) -> Option<PathBuf> {
        if !is_valid_frame_id(frame_id) {
            return None;
        }

        Some(self.archive_dir().join(format!("{frame_id}.png")))
    }

    async fn read_archive_frame(&self, frame_id: &str) -> io::Result<Vec<u8>> {
        let Some(path) = self.archive_path(frame_id) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unknown archive frame",
            ));
        };

        tokio::fs::read(path).await
    }

    async fn archive_frame_ids(&self) -> BTreeSet<String> {
        let mut frames = BTreeSet::new();
        let Ok(mut entries) = tokio::fs::read_dir(self.archive_dir()).await else {
            return frames;
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".png") else {
                continue;
            };
            if is_valid_frame_id(stem) {
                frames.insert(stem.to_owned());
            }
        }

        frames
    }

    async fn timeline_snapshot(&self) -> Vec<TimelineFrameSnapshot> {
        let manifest = load_timeline_manifest(self.manifest_path())
            .await
            .unwrap_or_default();
        let available = self.archive_frame_ids().await;

        timeline_frames(manifest.frames, &available)
    }

    async fn sync_archive(&self) {
        info!("archive sync started");
        let start_time = Instant::now();
        let result = self.run_generator().await;
        let elapsed = start_time.elapsed();
        let mut runtime = self.runtime.lock().await;
        runtime.syncing = false;

        match result {
            Ok(metadata) => {
                let available = self.archive_frame_ids().await;
                let available_count = available.len();
                if let Some(metadata) = metadata {
                    info!(
                        duration_secs = elapsed.as_secs_f32(),
                        latest_frame = %metadata.id,
                        satellite_time = %metadata.satellite_time,
                        cached_frames = available_count,
                        "archive sync completed in {:.1}s: latest observation is {} ({}), {} total frame(s) in archive",
                        elapsed.as_secs_f32(),
                        metadata.id,
                        metadata.satellite_time,
                        available_count
                    );
                    runtime.cache = Some(metadata);
                } else {
                    info!(
                        duration_secs = elapsed.as_secs_f32(),
                        "archive sync completed in {:.1}s with no visible frames (nighttime or no coverage)",
                        elapsed.as_secs_f32()
                    );
                    runtime.cache = None;
                }
                runtime.last_error = None;
            }
            Err(error) => {
                error!(
                    %error,
                    duration_secs = elapsed.as_secs_f32(),
                    "archive sync failed after {:.1}s",
                    elapsed.as_secs_f32()
                );
                runtime.last_error = Some(format!("Archive sync failed: {error:#}"));
            }
        }
    }

    async fn run_generator(&self) -> Result<Option<FrameMetadata>> {
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

        let mut child = Command::new(&self.config.python)
            .arg(&self.config.processor)
            .arg("--output")
            .arg(self.image_path())
            .arg("--metadata")
            .arg(self.metadata_path())
            .arg("--manifest")
            .arg(self.manifest_path())
            .arg("--archive-dir")
            .arg(self.archive_dir())
            .env("EUMETSAT_CONSUMER_KEY", consumer_key)
            .env("EUMETSAT_CONSUMER_SECRET", consumer_secret)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start processor {}",
                    self.config.processor.display()
                )
            })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let stdout_task = tokio::spawn(async move {
            if let Some(stdout) = stdout {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        info!(target: "mtg::processor", "{trimmed}");
                    }
                }
            }
        });

        let stderr_task = tokio::spawn(async move {
            if let Some(stderr) = stderr {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        info!(target: "mtg::processor", "{trimmed}");
                    }
                }
            }
        });

        let status = child.wait().await.context("failed to wait for processor")?;
        let _ = tokio::join!(stdout_task, stderr_task);

        if !status.success() {
            anyhow::bail!("processor exited with {status}");
        }

        Ok(load_cached_metadata(&self.config.cache_dir).await)
    }
}

fn timeline_frames(
    frames: Vec<FrameMetadata>,
    available: &BTreeSet<String>,
) -> Vec<TimelineFrameSnapshot> {
    frames
        .into_iter()
        .filter(|frame| available.contains(&frame.id))
        .map(|frame| TimelineFrameSnapshot {
            image_url: format!("/image/frames/{}", frame.id),
            id: frame.id,
            generated_unix: frame.generated_unix,
            satellite_time: frame.satellite_time,
        })
        .collect()
}

async fn load_cached_metadata(cache_dir: &Path) -> Option<FrameMetadata> {
    if !cache_dir.join("latest.png").is_file() {
        return None;
    }

    let bytes = tokio::fs::read(cache_dir.join("latest.json")).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn load_timeline_manifest(cache_dir: PathBuf) -> Option<TimelineManifest> {
    let bytes = tokio::fs::read(cache_dir).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn is_valid_frame_id(frame_id: &str) -> bool {
    if frame_id.len() != 16 {
        return false;
    }

    let bytes = frame_id.as_bytes();
    bytes[..8].iter().all(|byte| byte.is_ascii_digit())
        && bytes[8] == b'T'
        && bytes[9..15].iter().all(|byte| byte.is_ascii_digit())
        && bytes[15] == b'Z'
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

    fn cached_frame(generated_unix: u64) -> RuntimeState {
        RuntimeState {
            cache: Some(FrameMetadata {
                id: "20260723T120007Z".to_owned(),
                generated_unix,
                satellite_time: "2026-07-23T12:00:07Z".to_owned(),
                product_id: "test-product".to_owned(),
            }),
            ..RuntimeState::default()
        }
    }

    #[test]
    fn first_sync_starts() {
        let mut state = RuntimeState::default();
        assert!(state.begin_sync(1_000, false));
        assert!(state.syncing);
    }

    #[test]
    fn debounce_blocks_rapid_syncs() {
        let mut state = cached_frame(1_000);
        assert!(state.begin_sync(2_000, false));
        state.syncing = false;
        assert!(!state.begin_sync(2_030, false));
    }

    #[test]
    fn forced_sync_ignores_debounce() {
        let mut state = RuntimeState {
            last_sync_started_unix: Some(2_000),
            ..RuntimeState::default()
        };
        assert!(state.begin_sync(2_030, true));
    }

    #[test]
    fn validates_frame_ids() {
        assert!(is_valid_frame_id("20260913T102523Z"));
        assert!(!is_valid_frame_id("../latest"));
        assert!(!is_valid_frame_id("2026-09-13T102523Z"));
    }

    #[test]
    fn timeline_filters_missing_frames() {
        let frames = vec![
            FrameMetadata {
                id: "20260723T120007Z".to_owned(),
                generated_unix: 1,
                satellite_time: "2026-07-23T12:00:07Z".to_owned(),
                product_id: "one".to_owned(),
            },
            FrameMetadata {
                id: "20260723T121007Z".to_owned(),
                generated_unix: 2,
                satellite_time: "2026-07-23T12:10:07Z".to_owned(),
                product_id: "two".to_owned(),
            },
        ];
        let available = BTreeSet::from(["20260723T121007Z".to_owned()]);

        let snapshots = timeline_frames(frames, &available);

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].id, "20260723T121007Z");
        assert_eq!(snapshots[0].image_url, "/image/frames/20260723T121007Z");
    }
}
