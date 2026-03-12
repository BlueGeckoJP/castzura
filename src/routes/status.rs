use std::{
    net::SocketAddr,
    sync::{Arc, atomic::Ordering},
};

use axum::{
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{Html, IntoResponse},
};

use crate::{AppState, html_escape};

pub async fn status_handler(
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
        .map(|ip| format!("<li>{}</li>", html_escape(ip)))
        .collect::<Vec<_>>()
        .join("\n");

    let client_rows = if clients.is_empty() {
        "<li><em>none</em></li>".to_string()
    } else {
        clients
            .iter()
            .map(|ip| format!(
                "<li>{}<form method=\"post\" action=\"/disconnect\" style=\"display:inline;margin:0\"><input type=\"hidden\" name=\"ip\" value=\"{}\"><button type=\"submit\" class=\"disconnect-btn\">disconnect</button></form></li>",
                html_escape(ip),
                html_escape(ip)
            ))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let log_rows = logs
        .iter()
        .rev()
        .map(|l| format!("<div>{}</div>", html_escape(l)))
        .collect::<Vec<_>>()
        .join("\n");

    let html = include_str!("../../templates/status.html")
        .replace("{{ffmpeg}}", ffmpeg)
        .replace("{{pw}}", pw)
        .replace("{{whitelist_rows}}", &whitelist_rows)
        .replace("{{client_rows}}", &client_rows)
        .replace("{{log_rows}}", &log_rows);

    Html(html).into_response()
}

fn bool_label(v: bool) -> &'static str {
    if v { "yes" } else { "no" }
}
