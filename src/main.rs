use axum::{Router, extract::MatchedPath, http::Request, routing::get};
use tower_http::trace::TraceLayer;
use tracing::info_span;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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

    let app = Router::new().route("/health", get(health)).layer(
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
