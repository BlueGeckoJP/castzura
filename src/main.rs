use std::sync::Arc;

use axum::{
    Router,
    extract::{
        MatchedPath, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::Request,
    response::IntoResponse,
    routing::get,
};
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::{info, info_span};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use webrtc::{
    api::{APIBuilder, media_engine::MediaEngine},
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::track_local_static_rtp::TrackLocalStaticRTP,
};

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

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(health))
        .fallback_service(ServeDir::new("static"))
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

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_websocket)
}

async fn handle_websocket(mut socket: WebSocket) {
    let mut m = MediaEngine::default();
    m.register_default_codecs().unwrap();
    let api = APIBuilder::new().with_media_engine(m).build();
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
        peer_conn.set_local_description(answer).await.unwrap();

        socket
            .send(Message::Text(
                serde_json::to_string(&peer_conn.local_description().await.unwrap())
                    .unwrap()
                    .into(),
            ))
            .await
            .unwrap();

        info!("WebRTC connection established");
    }
}
