mod auth;
mod config;
mod discord;
mod error;
mod matchmaking;
mod models;
mod state;
mod sweeper;
mod turn;

use axum::{
    Router,
    routing::{get, post},
};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env for local dev — no-op in Cloud Run where env vars come from Secret Manager
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "signaling_server=debug,tower_http=debug".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = config::Config::from_env()?;
    let state = AppState::new(config).await?;

    sweeper::spawn(state.clone());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        // Health check — Cloud Run requires a responsive /health
        .route("/health", get(|| async { "ok" }))
        // Discord OAuth
        .route("/auth/discord",          get(auth::discord_login))
        .route("/auth/discord/callback", get(auth::discord_callback))
        .route("/auth/me",               get(auth::me))
        // Matchmaking
        .route("/match/lfg",                    post(matchmaking::looking_for_game))
        .route("/match/status/:session_id",     get(matchmaking::match_status))
        .route("/match/cancel",                 post(matchmaking::cancel_queue))
        .route("/match/turn-ready/:session_id", post(matchmaking::turn_ready))
        .route("/match/peer-relay/:session_id", get(matchmaking::peer_relay))
        // ICE/signal stubs — no-ops now, ready for TURN fallback later
        .route("/signal/offer",                 post(matchmaking::signal_offer))
        .route("/signal/answer",                post(matchmaking::signal_answer))
        .route("/signal/candidate",             post(matchmaking::signal_candidate))
        .route("/signal/poll/:session_id",      get(matchmaking::signal_poll))
        // Match result — determine winner, forward to stats service
        .route("/match/result",                 post(matchmaking::match_result))
        // Spar rooms — join-to-spar via Discord RPC
        .route("/room/create",                  post(matchmaking::create_room))
        .route("/room/join/:room_id",           post(matchmaking::join_room))
        // Spectator relay — watching live matches
        .route("/spectate/push/:session_id",    post(matchmaking::spectator_push))
        .route("/spectate/state/:session_id",   get(matchmaking::spectator_state))
        // Live matches dashboard — community public feed
        .route("/matches/live",                 get(matchmaking::live_matches))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("Signaling server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    // Graceful shutdown on SIGTERM (Cloud Run sends this on revision swap).
    // Without this, in-flight requests are cut at deploy time.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => { s.recv().await; }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("ctrl-c received, shutting down"),
        _ = terminate => tracing::info!("SIGTERM received, shutting down"),
    }
}
