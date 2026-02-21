mod ffmpeg_encoder;
mod pw_source;

use std::sync::Arc;

use crate::pw_source::PwSource;
use axum::{
    Router,
    extract::{
        MatchedPath, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::Request,
    response::IntoResponse,
    routing::get,
};
use std::time::Instant;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::{error, info, info_span};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
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

#[derive(Clone)]
struct AppState {
    pw_source: PwSource,
    tx: crossbeam::channel::Sender<Vec<u8>>,
    rx: crossbeam::channel::Receiver<Vec<u8>>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
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
        .init();

    let (tx, rx) = crossbeam::channel::unbounded();

    let app_state = AppState {
        pw_source: PwSource::default(),
        tx,
        rx,
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(health))
        .fallback_service(ServeDir::new("static"))
        .with_state(Arc::new(app_state))
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
                let matched_path = request.extensions().get::<MatchedPath>().map(MatchedPath::as_str);

                info_span!("http_request", method = ?request.method(), matched_path, some_other_field = tracing::field::Empty)
        })
    );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    tracing::info!("Listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health() -> &'static str {
    "OK"
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if !state.pw_source.is_initialized() {
        info!("Initializing PipeWire stream...");
        let (node_id, fd) = PwSource::get_pw_node_id().await.unwrap();
        let mut pw_source = state.pw_source.clone();
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = pw_source
                .start_pw_stream(fd, node_id, state.tx.clone())
                .await
            {
                error!("Error in PipeWire stream: {:?}", e);
            }
        });
    }

    ws.on_upgrade(|socket| handle_websocket(socket, state))
}

async fn handle_websocket(mut socket: WebSocket, state: Arc<AppState>) {
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
}
