mod ffmpeg_encoder;
mod pw_source;

use std::{
    collections::{HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::pw_source::PwSource;
use axum::{
    Router,
    extract::{
        ConnectInfo, MatchedPath, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::Html,
    response::IntoResponse,
    routing::get,
};
use clap::Parser;
use std::time::Instant;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::{error, info, info_span};
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
use webrtc::{
    api::{APIBuilder, media_engine::MediaEngine},
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
    },
    rtp::{
        codecs::h264::H264Payloader,
        packetizer::{Packetizer, new_packetizer},
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::{TrackLocalWriter, track_local_static_rtp::TrackLocalStaticRTP},
};

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

#[derive(Clone)]
struct AppState {
    pw_source: PwSource,
    tx: crossbeam::channel::Sender<Vec<u8>>,
    rx: crossbeam::channel::Receiver<Vec<u8>>,
    whitelist: Vec<IpAddr>,
    connected_clients: Arc<Mutex<HashSet<IpAddr>>>,
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

    let (tx, rx) = crossbeam::channel::unbounded();

    let app_state = AppState {
        pw_source: PwSource::default(),
        tx,
        rx,
        whitelist: cli.whitelist,
        connected_clients: Arc::new(Mutex::new(HashSet::new())),
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

fn bool_label(v: bool) -> &'static str {
    if v { "yes" } else { "no" }
}

async fn status_handler(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if !addr.ip().is_loopback() {
        return StatusCode::FORBIDDEN.into_response();
    }

    let clients: Vec<String> = state
        .connected_clients
        .lock()
        .unwrap()
        .iter()
        .map(|ip| ip.to_string())
        .collect();

    let whitelist: Vec<String> = state.whitelist.iter().map(|ip| ip.to_string()).collect();

    let logs: Vec<String> = state.log_buffer.lock().unwrap().iter().cloned().collect();

    let ffmpeg = bool_label(state.ffmpeg_running.load(Ordering::Relaxed));
    let pw = bool_label(state.pw_running.load(Ordering::Relaxed));

    let whitelist_rows = whitelist
        .iter()
        .map(|ip| format!("<li>{ip}</li>"))
        .collect::<Vec<_>>()
        .join("\n");

    let client_rows = if clients.is_empty() {
        "<li><em>none</em></li>".to_string()
    } else {
        clients
            .iter()
            .map(|ip| format!("<li>{ip}</li>"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let log_rows = logs
        .iter()
        .rev()
        .map(|l| format!("<div>{}</div>", html_escape(l)))
        .collect::<Vec<_>>()
        .join("\n");

    let html = include_str!("../templates/status.html")
        .replace("{{ffmpeg}}", ffmpeg)
        .replace("{{pw}}", pw)
        .replace("{{whitelist_rows}}", &whitelist_rows)
        .replace("{{client_rows}}", &client_rows)
        .replace("{{log_rows}}", &log_rows);

    Html(html).into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

async fn status_raw_handler(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if !addr.ip().is_loopback() {
        return StatusCode::FORBIDDEN.into_response();
    }

    let clients: Vec<String> = state
        .connected_clients
        .lock()
        .unwrap()
        .iter()
        .map(|ip| ip.to_string())
        .collect();

    let whitelist: Vec<String> = state.whitelist.iter().map(|ip| ip.to_string()).collect();

    let logs: Vec<String> = state.log_buffer.lock().unwrap().iter().cloned().collect();

    axum::Json(serde_json::json!({
        "whitelist": whitelist,
        "connected_clients": clients,
        "ffmpeg_running": state.ffmpeg_running.load(Ordering::Relaxed),
        "pw_running": state.pw_running.load(Ordering::Relaxed),
        "logs": logs,
    }))
    .into_response()
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

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if !state.pw_source.is_initialized() {
        info!("Initializing PipeWire stream...");
        let (node_id, fd) = PwSource::get_pw_node_id().await.unwrap();
        let mut pw_source = state.pw_source.clone();
        let ffmpeg_running = state.ffmpeg_running.clone();
        let pw_running = state.pw_running.clone();
        let tx = state.tx.clone();
        tokio::spawn(async move {
            if let Err(e) = pw_source
                .start_pw_stream(fd, node_id, tx, ffmpeg_running, pw_running)
                .await
            {
                error!("Error in PipeWire stream: {:?}", e);
            }
        });
    }

    let client_ip = addr.ip();
    ws.on_upgrade(move |socket| handle_websocket(socket, state, client_ip))
}

async fn handle_websocket(mut socket: WebSocket, state: Arc<AppState>, client_ip: IpAddr) {
    state.connected_clients.lock().unwrap().insert(client_ip);
    info!("WebSocket client connected: {}", client_ip);
    let mut m = MediaEngine::default();
    m.register_default_codecs().unwrap();

    let mut setting_engine = webrtc::api::setting_engine::SettingEngine::default();
    setting_engine.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_setting_engine(setting_engine)
        .build();
    let config = RTCConfiguration::default();

    let peer_conn = Arc::new(api.new_peer_connection(config).await.unwrap());

    let video_track = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: "video/h264".to_string(),
            ..Default::default()
        },
        "video".to_string(),
        "webrtc-rs".to_string(),
    ));
    peer_conn
        .add_track(Arc::clone(&video_track) as _)
        .await
        .unwrap();

    if let Some(Ok(Message::Text(text))) = socket.recv().await {
        let offer = serde_json::from_str::<RTCSessionDescription>(&text).unwrap();
        peer_conn.set_remote_description(offer).await.unwrap();

        let answer = peer_conn.create_answer(None).await.unwrap();
        let mut gathering_complete = peer_conn.gathering_complete_promise().await;
        peer_conn.set_local_description(answer).await.unwrap();
        let _ = gathering_complete.recv().await;

        socket
            .send(Message::Text(
                serde_json::to_string(&peer_conn.local_description().await.unwrap())
                    .unwrap()
                    .into(),
            ))
            .await
            .unwrap();

        info!("WebRTC connection established");

        let mut packetizer = new_packetizer(
            1200,
            96,
            0x12345678,
            Box::new(H264Payloader::default()),
            Box::new(webrtc::rtp::sequence::new_random_sequencer()),
            90000,
        );

        let start = Instant::now();
        let mut last_rtp_ts: u32 = 0;

        while let Ok(data) = state.rx.recv() {
            // Calculate RTP timestamp increment based on real elapsed time (90kHz clock)
            let now_ts = (start.elapsed().as_secs_f64() * 90000.0) as u32;
            let samples = now_ts.wrapping_sub(last_rtp_ts).max(1);
            last_rtp_ts = now_ts;

            let packets = packetizer.packetize(&data.into(), samples).unwrap();

            for packet in packets {
                video_track.write_rtp(&packet).await.unwrap();
            }
        }
    }

    state.connected_clients.lock().unwrap().remove(&client_ip);
    info!("WebSocket client disconnected: {}", client_ip);
}
