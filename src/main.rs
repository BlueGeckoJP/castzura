mod ffmpeg_encoder;
mod pw_source;
mod routes;

use std::{
    collections::{HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex, atomic::AtomicBool},
};

use crate::{
    pw_source::PwSource,
    routes::{
        disconnect::disconnect_handler, status::status_handler, status_raw::status_raw_handler,
        ws::ws_handler,
    },
};
use axum::{
    Router,
    extract::{ConnectInfo, MatchedPath, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
};
use clap::Parser;
use serde::Deserialize;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::{error, info_span};
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

// --- Log buffer layer ---

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        }
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        }
    }
}

struct LogBufferLayer {
    buffer: Arc<Mutex<VecDeque<String>>>,
}

impl<S: tracing::Subscriber> Layer<S> for LogBufferLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let line = format!(
            "[{}] {}: {}",
            event.metadata().target(),
            event.metadata().level(),
            visitor.message
        );
        let mut buf = self.buffer.lock().unwrap();
        if buf.len() >= 200 {
            buf.pop_front();
        }
        buf.push_back(line);
    }
}

// --- CLI / AppState ---

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(short, long, value_name = "IP", required = true, num_args = 1..)]
    whitelist: Vec<IpAddr>,
}

#[derive(Deserialize)]
struct DisconnectForm {
    ip: String,
}

#[derive(Clone)]
struct AppState {
    pw_source: PwSource,
    tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    whitelist: Vec<IpAddr>,
    connected_clients: Arc<Mutex<HashSet<IpAddr>>>,
    disconnect_requests: Arc<Mutex<HashSet<IpAddr>>>,
    ffmpeg_running: Arc<AtomicBool>,
    pw_running: Arc<AtomicBool>,
    log_buffer: Arc<Mutex<VecDeque<String>>>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    if cli.whitelist.is_empty() {
        error!("Error: whitelist is empty. Specify at least one IP with --whitelist.");
        std::process::exit(1);
    }

    let log_buffer: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!(
                    "{}=debug,tower_http=debug,axum::rejection=trace",
                    env!("CARGO_CRATE_NAME")
                )
                .into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(LogBufferLayer {
            buffer: log_buffer.clone(),
        })
        .init();

    let (tx, _) = tokio::sync::broadcast::channel(512);

    let app_state = AppState {
        pw_source: PwSource,
        tx,
        whitelist: cli.whitelist,
        connected_clients: Arc::new(Mutex::new(HashSet::new())),
        disconnect_requests: Arc::new(Mutex::new(HashSet::new())),
        ffmpeg_running: Arc::new(AtomicBool::new(false)),
        pw_running: Arc::new(AtomicBool::new(false)),
        log_buffer,
    };

    let shared_state = Arc::new(app_state);

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(health_handler))
        .route("/status", get(status_handler))
        .route("/status/raw", get(status_raw_handler))
        .route("/disconnect", post(disconnect_handler))
        .fallback_service(ServeDir::new("static"))
        .layer(middleware::from_fn_with_state(
            shared_state.clone(),
            whitelist_middleware,
        ))
        .with_state(shared_state)
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
                let matched_path = request.extensions().get::<MatchedPath>().map(MatchedPath::as_str);

                info_span!("http_request", method = ?request.method(), matched_path, some_other_field = tracing::field::Empty)
        })
    );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    tracing::info!("Listening on {}", listener.local_addr()?);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

async fn health_handler() -> &'static str {
    "OK"
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

async fn whitelist_middleware(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> impl IntoResponse {
    if !state.whitelist.contains(&addr.ip()) {
        error!(
            "Rejected connection from {} on {}",
            addr.ip(),
            request.uri()
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await.into_response()
}
