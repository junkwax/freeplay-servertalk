use crate::{
    config::Config,
    models::{Challenge, LobbyChatEntry, LobbyPresence, QueueEntry, SparRoom, SpectatorFrame},
};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use std::sync::Arc;

/// One side of a not-yet-confirmed match result. We require both clients to
/// report identical scores within a short window before committing the result;
/// otherwise the first report wins and a malicious client can fabricate
/// outcomes for rating farming.
#[derive(Clone, Debug)]
pub struct PendingResult {
    pub reporter_discord_id: String,
    pub p1_score: u16,
    pub p2_score: u16,
    pub reported_at: DateTime<Utc>,
}

/// State of a confirmed/published match result. Used for client-poll-style
/// dedup ("did we already record this?") and to keep the room garbage-
/// collectable on a known timestamp.
#[derive(Clone, Debug)]
pub struct ConfirmedResult {
    pub committed_at: DateTime<Utc>,
}

/// Short-lived OAuth `state` parameter store. Maps a server-generated nonce
/// to the timestamp it was issued; the callback validates and consumes it
/// to prevent CSRF on the Discord OAuth flow.
#[derive(Clone, Debug)]
pub struct OAuthState {
    pub issued_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub queue: Arc<DashMap<String, QueueEntry>>,
    pub player_sessions: Arc<DashMap<String, String>>,
    /// result_id (`room_id#match_index`) → first half of a match result,
    /// awaiting the partner's report.
    pub pending_results: Arc<DashMap<String, PendingResult>>,
    /// result_id (`room_id#match_index`) → committed result. Acts as the
    /// "already recorded" sentinel the dedup path checks.
    pub confirmed_results: Arc<DashMap<String, ConfirmedResult>>,
    pub spar_rooms: Arc<DashMap<String, SparRoom>>,
    pub lobby_presence: Arc<DashMap<String, LobbyPresence>>,
    pub lobby_chat: Arc<DashMap<String, LobbyChatEntry>>,
    pub challenges: Arc<DashMap<String, Challenge>>,
    /// session_id → latest spectator frame pushed by a playing peer
    pub spectator_frames: Arc<DashMap<String, SpectatorFrame>>,
    /// nonce → issued-at timestamp for in-flight Discord OAuth CSRF tokens.
    pub oauth_states: Arc<DashMap<String, OAuthState>>,
}

impl AppState {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        Ok(Self {
            config: Arc::new(config),
            http,
            queue: Arc::new(DashMap::new()),
            player_sessions: Arc::new(DashMap::new()),
            pending_results: Arc::new(DashMap::new()),
            confirmed_results: Arc::new(DashMap::new()),
            spar_rooms: Arc::new(DashMap::new()),
            lobby_presence: Arc::new(DashMap::new()),
            lobby_chat: Arc::new(DashMap::new()),
            challenges: Arc::new(DashMap::new()),
            spectator_frames: Arc::new(DashMap::new()),
            oauth_states: Arc::new(DashMap::new()),
        })
    }
}
