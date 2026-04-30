use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

// ── Auth ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordUser {
    pub id: String,
    pub username: String,
    pub discriminator: String,
    pub avatar: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub username: String,
    pub exp: usize,
}

// ── Matchmaking ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LfgRequest {
    pub stun_endpoint: String,
    pub app_version: String,
    /// Hex string of the client's mk2.zip fingerprint. Players with different
    /// ROM hashes never pair.
    #[serde(default)]
    pub rom_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LfgResponse {
    pub session_id: String,
    pub status: MatchStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MatchStatus {
    Queued,
    Matched,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchStatusResponse {
    pub status: MatchStatus,
    pub match_info: Option<MatchInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchInfo {
    pub role: PlayerRole,
    pub peer_endpoint: String,
    pub punch_at_ms: i64,
    pub room_id: String,
    /// Peer's Discord username (for life bar HUD display).
    pub username: String,
    /// Optional TURN relay credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnCredentials>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCredentials {
    pub uri: String,
    pub username: String,
    pub password: String,
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PlayerRole {
    Host,
    Join,
}

// ── TURN address exchange ────────────────────────────────────────────────────
//
// After both clients fail their direct hole punch and open TURN allocations,
// each client posts its *relayed* address to the server. The server stores
// it on the QueueEntry. Either client can then poll for the peer's relayed
// address — once both are reported, GGRS reconnects to the peer's TURN
// allocation address.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnReadyRequest {
    /// "ip:port" of this client's TURN-relayed address (from the
    /// XOR-RELAYED-ADDRESS attribute in the AllocateSuccess response).
    pub relayed_addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRelayResponse {
    /// Peer's relayed address if they've reported it. None means "still waiting".
    pub peer_relayed_addr: Option<String>,
}

// ── Glicko-2 ranking ──────────────────────────────────────────────────────────

/// What the client sends when a match ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchResultRequest {
    pub session_id: String,
    pub p1_score: u16,
    pub p2_score: u16,
}

// ── Internal queue entry ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub session_id: String,
    pub discord_id: String,
    pub username: String,
    pub stun_endpoint: String,
    pub app_version: String,
    pub rom_hash: String,
    pub queued_at: DateTime<Utc>,
    pub match_info: Option<MatchInfo>,
    pub cancelled: bool,
    /// This client's TURN-relayed address, set once they call /turn-ready.
    /// The other side polls for it via /peer-relay.
    pub relayed_addr: Option<String>,
}

// ── Spar rooms (join-to-spar via Discord RPC) ──────────────────────────────────

/// A training-mode player advertising their availability for a sparring match.
/// Created via POST /room/create, advertised via Discord RPC join secret
/// (`xband://join/<room_id>`), and claimed via POST /room/join/:room_id.
#[derive(Debug, Clone)]
pub struct SparRoom {
    pub room_id: String,
    pub creator_discord_id: String,
    pub creator_username: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateRoomResponse {
    pub room_id: String,
    /// The session ID the creator should poll via GET /match/status.
    pub creator_session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRoomRequest {
    pub stun_endpoint: String,
    pub app_version: String,
    #[serde(default)]
    pub rom_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRoomRequest {
    pub stun_endpoint: String,
    pub app_version: String,
    #[serde(default)]
    pub rom_hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JoinRoomResponse {
    pub session_id: String,
    pub match_info: MatchInfo,
}

// ── Spectator relay ────────────────────────────────────────────────────────
//
// Playing peers push confirmed frame data (savestate + recent inputs)
// so spectators can reconstruct the view. Spectators poll until they
// receive the state, load the savestate, and replay inputs to render.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectatorPushRequest {
    pub savestate: String,
    pub inputs: String,
    pub frame: u32,
    #[serde(default)]
    pub score_p1: u16,
    #[serde(default)]
    pub score_p2: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpectatorStateResponse {
    pub savestate: String,
    pub inputs: String,
    pub frame: u32,
    pub score_p1: u16,
    pub score_p2: u16,
    pub p1_username: String,
    pub p2_username: String,
}

#[derive(Debug, Clone)]
pub struct SpectatorFrame {
    pub savestate: String,
    pub inputs: String,
    pub frame: u32,
    pub score_p1: u16,
    pub score_p2: u16,
    pub p1_username: String,
    pub p2_username: String,
    pub updated_at: DateTime<Utc>,
}

// ── Live matches dashboard ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct LiveMatch {
    pub room_id: String,
    pub p1_username: String,
    pub p2_username: String,
    pub score_p1: u16,
    pub score_p2: u16,
    pub started_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveMatchesResponse {
    pub matches: Vec<LiveMatch>,
}
