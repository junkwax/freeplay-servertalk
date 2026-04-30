use axum::{extract::{Path, State}, response::Json};
use axum_extra::{headers::{Authorization, authorization::Bearer}, TypedHeader};
use chrono::Utc;
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::{auth::verify_token, discord, error::AppError, models::*, state::AppState, turn};

const TURN_TTL_SECS: u64 = 3600;

// ── POST /match/lfg ───────────────────────────────────────────────────────────

pub async fn looking_for_game(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Json(req): Json<LfgRequest>,
) -> Result<Json<LfgResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;

    if req.stun_endpoint.is_empty() || !req.stun_endpoint.contains(':') {
        return Err(AppError::BadRequest("Invalid stun_endpoint".into()));
    }

    if let Some(old_sid) = state.player_sessions.get(&claims.sub).map(|s| s.clone()) {
        if let Some((_, old)) = state.queue.remove(&old_sid) {
            if old.match_info.is_some() && !old.cancelled {
                tracing::warn!(
                    "{} re-queued while still matched (sid={}); peer will see a stale session",
                    claims.username, old_sid
                );
            }
        }
    }

    let session_id = Uuid::new_v4().to_string();

    // Pair only with someone matching our app_version AND rom_hash.
    // Snapshot candidate keys first (releasing iter shard locks), then claim
    // one atomically via get_mut + re-check. This closes the TOCTOU window
    // between find and get_mut, and avoids holding an iter while taking a
    // mutable shard lock (which dashmap will deadlock on).
    let candidate_keys: Vec<String> = state.queue.iter()
        .filter(|e| {
            !e.cancelled
                && e.match_info.is_none()
                && e.discord_id != claims.sub
                && e.app_version == req.app_version
                && e.rom_hash == req.rom_hash
        })
        .map(|e| e.key().clone())
        .collect();

    let mut claimed = None;
    for k in candidate_keys {
        let Some(mut opp_ref) = state.queue.get_mut(&k) else { continue };
        // Re-check under the mut lock — another request may have just claimed them.
        if opp_ref.cancelled || opp_ref.match_info.is_some() {
            continue;
        }

        let room_id = Uuid::new_v4().to_string();
        let punch_at_ms = (Utc::now() + chrono::Duration::milliseconds(2500)).timestamp_millis();
        let turn_creds = mint_turn_for_room(&state, &room_id);

        opp_ref.match_info = Some(MatchInfo {
            role: PlayerRole::Host,
            peer_endpoint: req.stun_endpoint.clone(),
            punch_at_ms,
            room_id: room_id.clone(),
            username: claims.username.clone(),
            turn: turn_creds.clone(),
        });

        claimed = Some((
            opp_ref.username.clone(),
            opp_ref.stun_endpoint.clone(),
            room_id,
            punch_at_ms,
            turn_creds,
        ));
        break;
    }

    if let Some((opp_username, opp_stun, room_id, punch_at_ms, turn_creds)) = claimed {
        state.queue.insert(session_id.clone(), QueueEntry {
            session_id: session_id.clone(),
            discord_id: claims.sub.clone(),
            username: claims.username.clone(),
            stun_endpoint: req.stun_endpoint.clone(),
            app_version: req.app_version.clone(),
            rom_hash: req.rom_hash.clone(),
            queued_at: Utc::now(),
            match_info: Some(MatchInfo {
                role: PlayerRole::Join,
                peer_endpoint: opp_stun,
                punch_at_ms,
                room_id,
                username: opp_username.clone(),
                turn: turn_creds,
            }),
            cancelled: false,
            relayed_addr: None,
        });

        state.player_sessions.insert(claims.sub.clone(), session_id.clone());

        let (s, u1, u2) = (state.clone(), claims.username.clone(), opp_username.clone());
        tokio::spawn(async move { discord::notify_matched(&s, &u1, &u2).await });

        tracing::info!("Matched {} vs {} (rom={})",
            claims.username, opp_username, &req.rom_hash);
        Ok(Json(LfgResponse { session_id, status: MatchStatus::Matched }))

    } else {
        state.queue.insert(session_id.clone(), QueueEntry {
            session_id: session_id.clone(),
            discord_id: claims.sub.clone(),
            username: claims.username.clone(),
            stun_endpoint: req.stun_endpoint,
            app_version: req.app_version,
            rom_hash: req.rom_hash.clone(),
            queued_at: Utc::now(),
            match_info: None,
            cancelled: false,
            relayed_addr: None,
        });
        state.player_sessions.insert(claims.sub.clone(), session_id.clone());

        let (s, u) = (state.clone(), claims.username.clone());
        tokio::spawn(async move { discord::notify_lfg(&s, &u).await });

        tracing::info!("Queued {} (rom={})", claims.username, &req.rom_hash);
        Ok(Json(LfgResponse { session_id, status: MatchStatus::Queued }))
    }
}

fn mint_turn_for_room(state: &AppState, room_id: &str) -> Option<TurnCredentials> {
    if !state.config.turn_enabled() {
        return None;
    }
    let ip = state.config.turn_server_ip.as_ref()?;
    let secret = state.config.turn_shared_secret.as_ref()?;
    Some(turn::mint_credentials(secret, ip, room_id, TURN_TTL_SECS))
}

// ── GET /match/status/:session_id ─────────────────────────────────────────────

pub async fn match_status(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(session_id): Path<String>,
) -> Result<Json<MatchStatusResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;

    let entry = state.queue.get(&session_id)
        .ok_or_else(|| AppError::NotFound("Session not found".into()))?;

    if entry.discord_id != claims.sub {
        return Err(AppError::Unauthorized("Session does not belong to you".into()));
    }

    if entry.cancelled {
        return Ok(Json(MatchStatusResponse { status: MatchStatus::Cancelled, match_info: None }));
    }

    if let Some(info) = &entry.match_info {
        Ok(Json(MatchStatusResponse { status: MatchStatus::Matched, match_info: Some(info.clone()) }))
    } else {
        Ok(Json(MatchStatusResponse { status: MatchStatus::Queued, match_info: None }))
    }
}

// ── POST /match/cancel ────────────────────────────────────────────────────────

pub async fn cancel_queue(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    if let Some(sid) = state.player_sessions.get(&claims.sub) {
        if let Some(mut e) = state.queue.get_mut(sid.as_str()) {
            e.cancelled = true;
        }
        state.player_sessions.remove(&claims.sub);
    }
    Ok(Json(json!({ "ok": true })))
}

// ── POST /match/turn-ready/:session_id ────────────────────────────────────────
//
// Called by the client after it has opened a TURN allocation. The relayed_addr
// is the XOR-RELAYED-ADDRESS the TURN server returned. The peer polls
// /match/peer-relay/:session_id to discover it.

pub async fn turn_ready(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(session_id): Path<String>,
    Json(req): Json<TurnReadyRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;

    if req.relayed_addr.is_empty() || !req.relayed_addr.contains(':') {
        return Err(AppError::BadRequest("Invalid relayed_addr".into()));
    }

    let mut entry = state.queue.get_mut(&session_id)
        .ok_or_else(|| AppError::NotFound("Session not found".into()))?;

    if entry.discord_id != claims.sub {
        return Err(AppError::Unauthorized("Session does not belong to you".into()));
    }

    entry.relayed_addr = Some(req.relayed_addr.clone());
    tracing::info!("[turn-ready] {} relayed_addr={}",
        entry.username, req.relayed_addr);

    Ok(Json(json!({ "ok": true })))
}

// ── GET /match/peer-relay/:session_id ────────────────────────────────────────
//
// Returns the peer's relayed address if they've called /turn-ready. The
// client polls this every ~500ms after registering its own address until
// the peer's appears, then reconnects GGRS to the relayed address.

pub async fn peer_relay(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(session_id): Path<String>,
) -> Result<Json<PeerRelayResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;

    let entry = state.queue.get(&session_id)
        .ok_or_else(|| AppError::NotFound("Session not found".into()))?;

    if entry.discord_id != claims.sub {
        return Err(AppError::Unauthorized("Session does not belong to you".into()));
    }

    let info = entry.match_info.as_ref()
        .ok_or_else(|| AppError::BadRequest("Not matched yet".into()))?;
    let room_id = info.room_id.clone();
    drop(entry); // release the dashmap borrow before the next iter

    // Find the OTHER player in the same room
    let peer_relayed = state.queue.iter()
        .find(|e| {
            e.discord_id != claims.sub
                && e.match_info.as_ref().map(|m| &m.room_id) == Some(&room_id)
        })
        .and_then(|e| e.relayed_addr.clone());

    Ok(Json(PeerRelayResponse { peer_relayed_addr: peer_relayed }))
}

// ── Signal stubs (kept for future ICE support) ───────────────────────────────

pub async fn signal_offer(
    State(_): State<AppState>,
    TypedHeader(_): TypedHeader<Authorization<Bearer>>,
) -> Result<Json<serde_json::Value>, AppError> { Ok(Json(json!({ "ok": true }))) }

pub async fn signal_answer(
    State(_): State<AppState>,
    TypedHeader(_): TypedHeader<Authorization<Bearer>>,
) -> Result<Json<serde_json::Value>, AppError> { Ok(Json(json!({ "ok": true }))) }

pub async fn signal_candidate(
    State(_): State<AppState>,
    TypedHeader(_): TypedHeader<Authorization<Bearer>>,
) -> Result<Json<serde_json::Value>, AppError> { Ok(Json(json!({ "ok": true }))) }

pub async fn signal_poll(
    State(_): State<AppState>,
    TypedHeader(_): TypedHeader<Authorization<Bearer>>,
    Path(_): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> { Ok(Json(json!({ "candidates": [] }))) }

// ── POST /match/result ─────────────────────────────────────────────────────
//
// Both clients report the same synced-GGRS outcome {session_id, p1_score, p2_score}.
// P1 in the game RAM = Host (local_handle 0), P2 = Join (local_handle 1).
// The server deduplicates by room_id, determines the winner, then forwards the
// result to the stats service for Glicko-2 rating update + leaderboard storage.

#[derive(Debug, Serialize)]
struct StatsForward {
    room_id: String,
    winner_id: String,
    loser_id: String,
    winner_score: u16,
    loser_score: u16,
    rom_hash: String,
    winner_username: String,
    loser_username: String,
}

pub async fn match_result(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Json(req): Json<MatchResultRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;

    let entry = state.queue.get(&req.session_id)
        .ok_or_else(|| AppError::NotFound("Session not found".into()))?;

    if entry.discord_id != claims.sub {
        return Err(AppError::Unauthorized("Session does not belong to you".into()));
    }

    let info = entry.match_info.as_ref()
        .ok_or_else(|| AppError::BadRequest("Match not yet started".into()))?;
    let room_id = info.room_id.clone();
    let own_role = info.role.clone();
    let own_discord_id = entry.discord_id.clone();
    let own_username = entry.username.clone();
    let rom_hash = entry.rom_hash.clone();
    drop(entry);

    // Dedup: both clients report the same outcome via synced GGRS RAM.
    if state.match_results.contains_key(&room_id) {
        return Ok(Json(json!({ "ok": true, "already_recorded": true })));
    }

    // Find the opponent's entry in the same room.
    let (opp_discord_id, opp_role, opp_username) = state.queue.iter()
        .find(|e| {
            e.discord_id != own_discord_id
                && e.match_info.as_ref().map(|m| &m.room_id) == Some(&room_id)
        })
        .map(|e| {
            (e.discord_id.clone(),
             e.match_info.as_ref().map(|m| m.role.clone()),
             e.username.clone())
        })
        .ok_or_else(|| AppError::BadRequest("Opponent not found in queue".into()))?;

    let opp_role = opp_role
        .ok_or_else(|| AppError::BadRequest("Opponent match info missing".into()))?;

    // Determine winner. Host = P1 in RAM (local_handle 0), Join = P2.
    let (host_id, join_id) = match (&own_role, &opp_role) {
        (PlayerRole::Host, PlayerRole::Join) => (own_discord_id.clone(), opp_discord_id.clone()),
        (PlayerRole::Join, PlayerRole::Host) => (opp_discord_id.clone(), own_discord_id.clone()),
        _ => return Err(AppError::BadRequest("Unexpected role pair".into())),
    };

    let host_won = req.p1_score > req.p2_score;

    let (winner_id, loser_id, winner_score, loser_score, winner_username, loser_username) = if host_won {
        let w_username = if own_role == PlayerRole::Host { own_username.clone() } else { opp_username.clone() };
        let l_username = if own_role == PlayerRole::Host { opp_username.clone() } else { own_username.clone() };
        (host_id.clone(), join_id.clone(), req.p1_score, req.p2_score, w_username, l_username)
    } else {
        let w_username = if own_role == PlayerRole::Join { own_username.clone() } else { opp_username.clone() };
        let l_username = if own_role == PlayerRole::Join { opp_username.clone() } else { own_username.clone() };
        (join_id.clone(), host_id.clone(), req.p2_score, req.p1_score, w_username, l_username)
    };

    state.match_results.insert(room_id.clone(), ());
    tracing::info!(
        "Match result: {} beat {} {}:{} (room={})",
        winner_id, loser_id, winner_score, loser_score, room_id
    );

    // Forward to stats service if configured.
    if let (Some(url), Some(key)) = (&state.config.stats_service_url, &state.config.stats_api_key) {
        let payload = StatsForward {
            room_id,
            winner_id,
            loser_id,
            winner_score,
            loser_score,
            rom_hash,
            winner_username,
            loser_username,
        };
        let state_clone = state.clone();
        let url_clone = url.clone();
        let key_clone = key.clone();
        tokio::spawn(async move {
            match state_clone.http
                .post(&url_clone)
                .header("Authorization", format!("Bearer {key_clone}"))
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) => {
                    if resp.status().is_success() {
                        tracing::debug!("Forwarded match result to stats service");
                    } else {
                        tracing::warn!("Stats service returned {}", resp.status());
                    }
                }
                Err(e) => tracing::warn!("Failed to forward to stats service: {e}"),
            }
        });
    }

    Ok(Json(json!({ "ok": true })))
}

// ── POST /room/create ──────────────────────────────────────────────────────────
//
// Advertise a joinable sparring session. Called when a player enters Training
// mode and wants to appear as "Join"able on Discord. Returns the room_id
// (which becomes the xband://join/<room_id> RPC secret) and a creator_session_id
// that the creator polls via GET /match/status until someone joins.

pub async fn create_room(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Json(req): Json<CreateRoomRequest>,
) -> Result<Json<CreateRoomResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;

    if req.stun_endpoint.is_empty() || !req.stun_endpoint.contains(':') {
        return Err(AppError::BadRequest("Invalid stun_endpoint".into()));
    }

    let room_id = Uuid::new_v4().to_string();
    let creator_session_id = Uuid::new_v4().to_string();

    // Register the room so someone can join it
    state.spar_rooms.insert(room_id.clone(), SparRoom {
        room_id: room_id.clone(),
        creator_discord_id: claims.sub.clone(),
        creator_username: claims.username.clone(),
        created_at: Utc::now(),
    });

    // Create a placeholder queue entry so the creator can poll /match/status
    state.queue.insert(creator_session_id.clone(), QueueEntry {
        session_id: creator_session_id.clone(),
        discord_id: claims.sub.clone(),
        username: claims.username.clone(),
        stun_endpoint: req.stun_endpoint, // shared with joiner on match
        app_version: req.app_version,
        rom_hash: req.rom_hash,
        queued_at: Utc::now(),
        match_info: None,
        cancelled: false,
        relayed_addr: None,
    });

    state.player_sessions.insert(claims.sub.clone(), creator_session_id.clone());

    tracing::info!("Room {} created by {}", room_id, claims.username);
    Ok(Json(CreateRoomResponse { room_id, creator_session_id }))
}

// ── POST /room/join/:room_id ───────────────────────────────────────────────────
//
// Claim an existing spar room. Called when a friend clicks "Join" on the
// Discord profile card and their client parses the xband://join/<room_id> URL.

pub async fn join_room(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(room_id): Path<String>,
    Json(req): Json<JoinRoomRequest>,
) -> Result<Json<JoinRoomResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;

    if req.stun_endpoint.is_empty() || !req.stun_endpoint.contains(':') {
        return Err(AppError::BadRequest("Invalid stun_endpoint".into()));
    }

    // Find and consume the room — atomically remove so it can't be joined twice
    let room = state.spar_rooms.remove(&room_id)
        .map(|(_, r)| r)
        .ok_or_else(|| AppError::NotFound("Room not found or already joined".into()))?;

    if room.creator_discord_id == claims.sub {
        return Err(AppError::BadRequest("Cannot join your own room".into()));
    }

    // Find the creator's queue entry
    let creator_session_id = state.player_sessions.get(&room.creator_discord_id)
        .map(|s| s.clone())
        .ok_or_else(|| AppError::NotFound("Room creator is no longer available".into()))?;

    let Some(mut creator_entry) = state.queue.get_mut(&creator_session_id) else {
        return Err(AppError::NotFound("Room creator session expired".into()));
    };
    if creator_entry.cancelled || creator_entry.match_info.is_some() {
        return Err(AppError::NotFound("Room creator is no longer in queue".into()));
    }

    let joiner_session_id = Uuid::new_v4().to_string();
    let match_room_id = Uuid::new_v4().to_string();
    let punch_at_ms = (Utc::now() + chrono::Duration::milliseconds(2500)).timestamp_millis();
    let turn_creds = mint_turn_for_room(&state, &match_room_id);

    // Copy the creator's STUN endpoint before we overwrite their entry
    let creator_stun = creator_entry.stun_endpoint.clone();

    // Update creator's entry: they are Host, seeing the joiner as peer
    creator_entry.match_info = Some(MatchInfo {
        role: PlayerRole::Host,
        peer_endpoint: req.stun_endpoint.clone(),
        punch_at_ms,
        room_id: match_room_id.clone(),
        username: claims.username.clone(),
        turn: turn_creds.clone(),
    });

    // Create joiner's entry: they are Join
    state.queue.insert(joiner_session_id.clone(), QueueEntry {
        session_id: joiner_session_id.clone(),
        discord_id: claims.sub.clone(),
        username: claims.username.clone(),
        stun_endpoint: req.stun_endpoint.clone(),
        app_version: req.app_version.clone(),
        rom_hash: req.rom_hash.clone(),
        queued_at: Utc::now(),
        match_info: Some(MatchInfo {
            role: PlayerRole::Join,
            peer_endpoint: creator_stun.clone(),
            punch_at_ms,
            room_id: match_room_id.clone(),
            username: room.creator_username.clone(),
            turn: turn_creds.clone(),
        }),
        cancelled: false,
        relayed_addr: None,
    });

    state.player_sessions.insert(claims.sub.clone(), joiner_session_id.clone());

    let (s, u1, u2) = (state.clone(), room.creator_username.clone(), claims.username.clone());
    tokio::spawn(async move { discord::notify_matched(&s, &u1, &u2).await });

    tracing::info!("Room {} joined: {} (creator) vs {} (joiner)",
        room_id, room.creator_username, claims.username);

    Ok(Json(JoinRoomResponse {
        session_id: joiner_session_id,
        match_info: MatchInfo {
            role: PlayerRole::Join,
            peer_endpoint: creator_stun,
            punch_at_ms,
            room_id: match_room_id,
            username: room.creator_username.clone(),
            turn: turn_creds,
        },
    }))
}

// ── Spectator relay ────────────────────────────────────────────────────────

/// Playing peer pushes their latest confirmed frame so spectators can
/// reconstruct the view. Called periodically during the match.
pub async fn spectator_push(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<SpectatorPushRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Read player names from the queue entry
    let (p1_username, p2_username) = if let Some(entry) = state.queue.get(&session_id) {
        let p1 = entry.username.clone();
        // Find opponent's entry sharing the same room_id
        let room_id = entry.match_info.as_ref().map(|m| m.room_id.clone()).unwrap_or_default();
        let p2 = state.queue.iter()
            .find(|e| {
                e.match_info.as_ref()
                    .map(|m| m.room_id == room_id)
                    .unwrap_or(false)
                    && e.discord_id != entry.discord_id
            })
            .map(|e| e.username.clone())
            .unwrap_or_else(|| "Opponent".to_string());
        (p1, p2)
    } else {
        ("Player 1".to_string(), "Player 2".to_string())
    };

    state.spectator_frames.insert(session_id.clone(), SpectatorFrame {
        savestate: req.savestate,
        inputs: req.inputs,
        frame: req.frame,
        score_p1: req.score_p1,
        score_p2: req.score_p2,
        p1_username,
        p2_username,
        updated_at: Utc::now(),
    });

    tracing::debug!("Spectator frame pushed for session {session_id}: frame={}", req.frame);
    Ok(Json(json!({ "ok": true })))
}

/// A spectator client (from Discord "Spectate" button) polls for the
/// latest frame data. Returns `null` if no frame has been pushed yet.
pub async fn spectator_state(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<Option<SpectatorStateResponse>>, AppError> {
    let frame = state.spectator_frames.get(&session_id);
    match frame {
        Some(f) => Ok(Json(Some(SpectatorStateResponse {
            savestate: f.savestate.clone(),
            inputs: f.inputs.clone(),
            frame: f.frame,
            score_p1: f.score_p1,
            score_p2: f.score_p2,
            p1_username: f.p1_username.clone(),
            p2_username: f.p2_username.clone(),
        }))),
        None => Ok(Json(None)),
    }
}

// ── GET /matches/live ────────────────────────────────────────────────────────
//
// Returns all currently active matches (matched but not yet cancelled or finished).
// The client polls this to show live match cards in the dashboard sidebar.
// No auth required — public community info.

pub async fn live_matches(
    State(state): State<AppState>,
) -> Result<Json<LiveMatchesResponse>, AppError> {
    let mut matches = Vec::new();

    for entry in state.queue.iter() {
        let (_sid, q) = entry.pair();
        if q.cancelled { continue; }
        let info = if let Some(ref info) = q.match_info { info } else { continue; };

        // Look up the opponent's username from the match_info
        let p1_username = if info.role == PlayerRole::Host {
            q.username.clone()
        } else {
            info.username.clone()
        };
        let p2_username = if info.role == PlayerRole::Host {
            info.username.clone()
        } else {
            q.username.clone()
        };

        // Try to get live scores from spectator frames
        let (score_p1, score_p2) = state.spectator_frames
            .get(&info.room_id)
            .map(|f| (f.score_p1, f.score_p2))
            .unwrap_or((0, 0));

        matches.push(LiveMatch {
            room_id: info.room_id.clone(),
            p1_username,
            p2_username,
            score_p1,
            score_p2,
            started_at: q.queued_at.format("%H:%M").to_string(),
        });
    }

    // Deduplicate by room_id (both queue entries share the same room_id)
    let mut seen = std::collections::HashSet::new();
    matches.retain(|m| seen.insert(m.room_id.clone()));

    Ok(Json(LiveMatchesResponse { matches }))
}
