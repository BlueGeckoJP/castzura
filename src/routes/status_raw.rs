use std::{
    net::SocketAddr,
    sync::{Arc, atomic::Ordering},
};

use axum::{
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::IntoResponse,
};

use crate::AppState;

pub async fn status_raw_handler(
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
