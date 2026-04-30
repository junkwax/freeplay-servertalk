use std::sync::Arc;
use dashmap::DashMap;
use crate::{config::Config, models::{QueueEntry, SparRoom, SpectatorFrame}};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub queue: Arc<DashMap<String, QueueEntry>>,
    pub player_sessions: Arc<DashMap<String, String>>,
    pub match_results: Arc<DashMap<String, ()>>,
    pub spar_rooms: Arc<DashMap<String, SparRoom>>,
    /// session_id → latest spectator frame pushed by a playing peer
    pub spectator_frames: Arc<DashMap<String, SpectatorFrame>>,
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
            match_results: Arc::new(DashMap::new()),
            spar_rooms: Arc::new(DashMap::new()),
            spectator_frames: Arc::new(DashMap::new()),
        })
    }
}
