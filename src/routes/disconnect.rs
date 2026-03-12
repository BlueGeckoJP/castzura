use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use axum::{
    Form,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use tracing::info;

use crate::{AppState, DisconnectForm};

pub async fn disconnect_handler(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Form(form): Form<DisconnectForm>,
) -> impl IntoResponse {
    if !addr.ip().is_loopback() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Ok(ip) = form.ip.parse::<IpAddr>() {
        state.disconnect_requests.lock().unwrap().insert(ip);
        info!("Disconnect requested for: {}", ip);
    }
    Redirect::to("/status").into_response()
}
