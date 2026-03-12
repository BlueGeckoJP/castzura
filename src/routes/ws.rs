use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

use axum::{
    extract::{
        ConnectInfo, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::IntoResponse,
};
use tracing::{error, info};
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

use crate::{AppState, pw_source::PwSource};

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    if state
        .pw_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        info!("Initializing PipeWire stream...");
        let (node_id, fd) = PwSource::get_pw_node_id().await.unwrap();
        let mut pw_source = state.pw_source.clone();
        let ffmpeg_running = state.ffmpeg_running.clone();
        let pw_running = state.pw_running.clone();
        let tx = state.tx.clone();
        tokio::spawn(async move {
            if let Err(e) = pw_source
                .start_pw_stream(fd, node_id, tx, ffmpeg_running, pw_running.clone())
                .await
            {
                error!("Error in PipeWire stream: {:?}", e);
                pw_running.store(false, Ordering::SeqCst);
            }
        });
    } else {
        info!("PipeWire stream is already running, skipping initialization");
    }

    let client_ip = addr.ip();
    ws.on_upgrade(move |socket| handle_websocket(socket, state, client_ip))
}

async fn handle_websocket(mut socket: WebSocket, state: Arc<AppState>, client_ip: IpAddr) {
    state.connected_clients.lock().unwrap().insert(client_ip);
    info!("WebSocket client connected: {}", client_ip);
    // Subscribe before WebRTC handshake so no frames are missed
    let mut rx = state.tx.subscribe();
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

        loop {
            let data = match rx.recv().await {
                Ok(d) => d,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Video receiver lagged by {} frames, skipping", n);
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };

            if state.disconnect_requests.lock().unwrap().remove(&client_ip) {
                info!("Disconnecting client by request: {}", client_ip);
                break;
            }
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
