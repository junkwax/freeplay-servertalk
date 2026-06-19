use axum::{
    extract::{Path, State},
    response::Json,
};
use axum_extra::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use chrono::Utc;
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::{
    auth::verify_token,
    discord,
    error::AppError,
    incidents::{record_server_incident, ServerIncident},
    models::*,
    state::{AppState, ConfirmedResult, PendingResult},
    turn,
};

const TURN_TTL_SECS: u64 = 3600;
const LOBBY_CHAT_MAX_CHARS: usize = 180;
const LOBBY_CHAT_VISIBLE: usize = 50;
const LOBBY_PRESENCE_VISIBLE_SECS: i64 = 90;

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
            // If the player was already in a matched session when they
            // re-queued, their previous opponent is still polling
            // /match/status with that room_id and would otherwise see
            // "Matched" forever pointing at a peer endpoint that no
            // longer exists. Explicitly mark the partner cancelled so
            // they get a clean error instead of a 10-second hole-punch
            // timeout. Without this, the player who DIDN'T re-queue
            // experiences "matchmaking timed out for no apparent
            // reason" — the symptom we saw with junkwaxc + sudden_recline.
            if let Some(info) = &old.match_info {
                if !old.cancelled {
                    let partner_keys: Vec<String> = state
                        .queue
                        .iter()
                        .filter(|e| {
                            e.discord_id != claims.sub
                                && e.match_info.as_ref().map(|m| &m.room_id) == Some(&info.room_id)
                        })
                        .map(|e| e.key().clone())
                        .collect();
                    for k in partner_keys {
                        if let Some(mut partner) = state.queue.get_mut(&k) {
                            partner.cancelled = true;
                            tracing::warn!(
                                "Cascading cancel to partner {} (sid={}) because {} re-queued from a stale match (room={})",
                                partner.username, k, claims.username, info.room_id,
                            );
                        }
                    }
                }
            }
        }
    }

    let session_id = Uuid::new_v4().to_string();

    // Pair only with someone matching our app_version AND rom_hash.
    // Snapshot candidate keys first (releasing iter shard locks), then claim
    // one atomically via get_mut + re-check. This closes the TOCTOU window
    // between find and get_mut, and avoids holding an iter while taking a
    // mutable shard lock (which dashmap will deadlock on).
    let candidate_keys: Vec<String> = state
        .queue
        .iter()
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
        let Some(mut opp_ref) = state.queue.get_mut(&k) else {
            continue;
        };
        // Re-check under the mut lock — another request may have just claimed them.
        if opp_ref.cancelled || opp_ref.match_info.is_some() {
            continue;
        }

        let room_id = Uuid::new_v4().to_string();
        let punch_at_ms = (Utc::now() + chrono::Duration::milliseconds(2500)).timestamp_millis();
        // Each peer gets a credential carrying ITS role. Relay HMAC is
        // <role>:<expiry>:<room_id>, so per-role minting is required.
        let host_creds = mint_turn_for_room(&state, &room_id, 0);
        let join_creds = mint_turn_for_room(&state, &room_id, 1);

        opp_ref.match_info = Some(MatchInfo {
            role: PlayerRole::Host,
            peer_endpoint: req.stun_endpoint.clone(),
            punch_at_ms,
            room_id: room_id.clone(),
            username: claims.username.clone(),
            turn: host_creds.clone(),
        });

        claimed = Some((
            opp_ref.username.clone(),
            opp_ref.stun_endpoint.clone(),
            room_id,
            punch_at_ms,
            join_creds,
        ));
        break;
    }

    if let Some((opp_username, opp_stun, room_id, punch_at_ms, join_creds)) = claimed {
        state.queue.insert(
            session_id.clone(),
            QueueEntry {
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
                    turn: join_creds,
                }),
                cancelled: false,
                relayed_addr: None,
            },
        );

        state
            .player_sessions
            .insert(claims.sub.clone(), session_id.clone());

        let (s, u1, u2) = (state.clone(), claims.username.clone(), opp_username.clone());
        tokio::spawn(async move { discord::notify_matched(&s, &u1, &u2).await });

        tracing::info!(
            "Matched {} vs {} (rom={})",
            claims.username,
            opp_username,
            &req.rom_hash
        );
        Ok(Json(LfgResponse {
            session_id,
            status: MatchStatus::Matched,
        }))
    } else {
        state.queue.insert(
            session_id.clone(),
            QueueEntry {
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
            },
        );
        state
            .player_sessions
            .insert(claims.sub.clone(), session_id.clone());

        let (s, u) = (state.clone(), claims.username.clone());
        tokio::spawn(async move { discord::notify_lfg(&s, &u).await });

        tracing::info!("Queued {} (rom={})", claims.username, &req.rom_hash);
        Ok(Json(LfgResponse {
            session_id,
            status: MatchStatus::Queued,
        }))
    }
}

/// Mint a relay credential for one peer in a match.
///
/// Each peer needs its OWN credential (with their role baked in) because
/// the relay HMACs `<role>:<expiry>:<room_id>`. Pre-relay (coturn era)
/// both peers shared one credential; the new design's per-role HMAC
/// means we mint twice per match.
fn mint_turn_for_room(state: &AppState, room_id: &str, role: u8) -> Option<TurnCredentials> {
    if !state.config.turn_enabled() {
        return None;
    }
    let ip = state.config.turn_server_ip.as_ref()?;
    let secret = state.config.turn_shared_secret.as_ref()?;
    Some(turn::mint_credentials(
        secret,
        ip,
        room_id,
        role,
        TURN_TTL_SECS,
    ))
}

// ── General lobby and lobby browser ───────────────────────────────────────────

pub async fn general_lobby(
    State(state): State<AppState>,
    auth: Option<TypedHeader<Authorization<Bearer>>>,
) -> Result<Json<GeneralLobbyResponse>, AppError> {
    if let Some(TypedHeader(auth)) = auth {
        if let Ok(claims) = verify_token(auth.token(), &state.config.jwt_secret) {
            touch_lobby_presence(&state, &claims, "online");
        }
    }

    ensure_lobby_ratings(&state);
    Ok(Json(general_lobby_snapshot(&state)))
}

/// Glicko rating cache TTL — re-fetch a player's rating from the stats service
/// at most this often.
const RATING_TTL_SECS: i64 = 300;

/// Spawn background rating fetches for any visible lobby player whose cached
/// rating is missing or stale. Non-blocking; the rating shows on a later
/// presence refresh once the fetch lands.
fn ensure_lobby_ratings(state: &AppState) {
    let Some(stats_url) = state.config.stats_service_url.clone() else {
        return;
    };
    let now = Utc::now();
    for entry in state.lobby_presence.iter() {
        if (now - entry.last_seen).num_seconds() > LOBBY_PRESENCE_VISIBLE_SECS {
            continue;
        }
        let pid = entry.player_id.clone();
        let fresh = state
            .ratings
            .get(&pid)
            .map(|r| (now - r.1).num_seconds() <= RATING_TTL_SECS)
            .unwrap_or(false);
        if fresh {
            continue;
        }
        let http = state.http.clone();
        let url = format!("{}/player/{}", stats_url.trim_end_matches('/'), pid);
        let ratings = state.ratings.clone();
        tokio::spawn(async move {
            if let Some(rating) = fetch_player_rating(&http, &url).await {
                ratings.insert(pid, (rating, Utc::now()));
            }
        });
    }
}

async fn fetch_player_rating(http: &reqwest::Client, url: &str) -> Option<i32> {
    let resp = http.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("rating")
        .and_then(|v| v.as_f64())
        .map(|r| r.round() as i32)
}

pub async fn lobby_chat(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Json(req): Json<LobbyChatRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    let message = normalize_lobby_chat(&req.message)?;
    touch_lobby_presence(&state, &claims, "chatting");

    let message_id = Uuid::new_v4().to_string();
    state.lobby_chat.insert(
        message_id.clone(),
        LobbyChatEntry {
            message_id,
            player_id: claims.sub,
            username: claims.username,
            message,
            created_at: Utc::now(),
        },
    );

    Ok(Json(json!({ "status": "ok" })))
}

pub async fn list_lobbies(
    State(state): State<AppState>,
) -> Result<Json<LobbyListResponse>, AppError> {
    let mut lobbies: Vec<LobbyRoomSummary> = state
        .spar_rooms
        .iter()
        .filter(|room| !room.private)
        .map(|room| lobby_room_summary(&room))
        .collect();
    lobbies.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    Ok(Json(LobbyListResponse { lobbies }))
}

pub async fn list_challenges(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
) -> Result<Json<ChallengeListResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    touch_lobby_presence(&state, &claims, "online");

    let mut challenges: Vec<ChallengeSummary> = state
        .challenges
        .iter()
        .filter_map(|entry| {
            let challenge = entry.value();
            if challenge.target_discord_id == claims.sub {
                Some(challenge_summary(challenge, "incoming"))
            } else if challenge.challenger_discord_id == claims.sub {
                Some(challenge_summary(challenge, "outgoing"))
            } else {
                None
            }
        })
        .collect();
    challenges.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.challenge_id.cmp(&b.challenge_id))
    });

    Ok(Json(ChallengeListResponse { challenges }))
}

pub async fn send_challenge(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Json(req): Json<ChallengeRequest>,
) -> Result<Json<ChallengeResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;

    if req.stun_endpoint.is_empty() || !req.stun_endpoint.contains(':') {
        return Err(AppError::BadRequest("Invalid stun_endpoint".into()));
    }
    if req.target_id == claims.sub {
        return Err(AppError::BadRequest("Cannot challenge yourself".into()));
    }

    let target = state
        .lobby_presence
        .get(&req.target_id)
        .map(|presence| (presence.player_id.clone(), presence.username.clone()))
        .ok_or_else(|| AppError::NotFound("Target player is not in the lobby".into()))?;

    let challenge_id = Uuid::new_v4().to_string();
    let challenger_session_id = Uuid::new_v4().to_string();

    state.queue.insert(
        challenger_session_id.clone(),
        QueueEntry {
            session_id: challenger_session_id.clone(),
            discord_id: claims.sub.clone(),
            username: claims.username.clone(),
            stun_endpoint: req.stun_endpoint.clone(),
            app_version: req.app_version.clone(),
            rom_hash: req.rom_hash.clone(),
            queued_at: Utc::now(),
            match_info: None,
            cancelled: false,
            relayed_addr: None,
        },
    );
    state
        .player_sessions
        .insert(claims.sub.clone(), challenger_session_id.clone());

    state.challenges.insert(
        challenge_id.clone(),
        Challenge {
            challenge_id: challenge_id.clone(),
            challenger_session_id: challenger_session_id.clone(),
            challenger_discord_id: claims.sub.clone(),
            challenger_username: claims.username.clone(),
            target_discord_id: target.0,
            target_username: target.1,
            format: req.format,
            stun_endpoint: req.stun_endpoint,
            app_version: req.app_version,
            rom_hash: req.rom_hash,
            created_at: Utc::now(),
        },
    );

    touch_lobby_presence(&state, &claims, "challenging");
    Ok(Json(ChallengeResponse {
        challenge_id,
        challenger_session_id,
        status: "pending".into(),
    }))
}

pub async fn accept_challenge(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(challenge_id): Path<String>,
    Json(req): Json<AcceptChallengeRequest>,
) -> Result<Json<JoinRoomResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;

    if req.stun_endpoint.is_empty() || !req.stun_endpoint.contains(':') {
        return Err(AppError::BadRequest("Invalid stun_endpoint".into()));
    }

    let challenge = state
        .challenges
        .remove(&challenge_id)
        .map(|(_, challenge)| challenge)
        .ok_or_else(|| AppError::NotFound("Challenge not found or expired".into()))?;

    if challenge.target_discord_id != claims.sub {
        state
            .challenges
            .insert(challenge.challenge_id.clone(), challenge);
        return Err(AppError::Unauthorized(
            "Challenge does not belong to you".into(),
        ));
    }
    if challenge.app_version != req.app_version || challenge.rom_hash != req.rom_hash {
        state
            .challenges
            .insert(challenge.challenge_id.clone(), challenge);
        return Err(AppError::BadRequest(
            "Challenge requires matching app version and ROM hash".into(),
        ));
    }

    let Some(mut challenger_entry) = state.queue.get_mut(&challenge.challenger_session_id) else {
        return Err(AppError::NotFound("Challenger session expired".into()));
    };
    if challenger_entry.cancelled || challenger_entry.match_info.is_some() {
        return Err(AppError::NotFound(
            "Challenger is no longer available".into(),
        ));
    }

    let acceptor_session_id = Uuid::new_v4().to_string();
    let match_room_id = Uuid::new_v4().to_string();
    let punch_at_ms = (Utc::now() + chrono::Duration::milliseconds(2500)).timestamp_millis();
    let host_creds = mint_turn_for_room(&state, &match_room_id, 0);
    let join_creds = mint_turn_for_room(&state, &match_room_id, 1);
    let challenger_stun = challenger_entry.stun_endpoint.clone();

    challenger_entry.match_info = Some(MatchInfo {
        role: PlayerRole::Host,
        peer_endpoint: req.stun_endpoint.clone(),
        punch_at_ms,
        room_id: match_room_id.clone(),
        username: claims.username.clone(),
        turn: host_creds,
    });
    drop(challenger_entry);

    state.queue.insert(
        acceptor_session_id.clone(),
        QueueEntry {
            session_id: acceptor_session_id.clone(),
            discord_id: claims.sub.clone(),
            username: claims.username.clone(),
            stun_endpoint: req.stun_endpoint.clone(),
            app_version: req.app_version,
            rom_hash: req.rom_hash,
            queued_at: Utc::now(),
            match_info: Some(MatchInfo {
                role: PlayerRole::Join,
                peer_endpoint: challenger_stun.clone(),
                punch_at_ms,
                room_id: match_room_id.clone(),
                username: challenge.challenger_username.clone(),
                turn: join_creds.clone(),
            }),
            cancelled: false,
            relayed_addr: None,
        },
    );
    state
        .player_sessions
        .insert(claims.sub.clone(), acceptor_session_id.clone());

    // Both players are now in a match. Drop every other pending challenge that
    // involves either of them (as challenger or target) so a second target
    // can't accept a now-busy player, and cancel the abandoned challengers'
    // queue entries so their clients see a clean "cancelled" instead of a
    // timeout. (The accepted challenge was already removed above.)
    let challenger_id = challenge.challenger_discord_id.clone();
    let acceptor_id = claims.sub.clone();
    let stale: Vec<(String, String)> = state
        .challenges
        .iter()
        .filter(|e| {
            let c = e.value();
            c.challenger_discord_id == challenger_id
                || c.target_discord_id == challenger_id
                || c.challenger_discord_id == acceptor_id
                || c.target_discord_id == acceptor_id
        })
        .map(|e| (e.key().clone(), e.value().challenger_session_id.clone()))
        .collect();
    for (cid, challenger_session) in stale {
        state.challenges.remove(&cid);
        if let Some(mut entry) = state.queue.get_mut(&challenger_session) {
            if entry.match_info.is_none() {
                entry.cancelled = true;
            }
        }
    }

    let (s, u1, u2) = (
        state.clone(),
        challenge.challenger_username.clone(),
        claims.username.clone(),
    );
    tokio::spawn(async move { discord::notify_matched(&s, &u1, &u2).await });

    tracing::info!(
        "Challenge {} accepted: {} vs {} ({:?})",
        challenge_id,
        challenge.challenger_username,
        claims.username,
        challenge.format
    );

    Ok(Json(JoinRoomResponse {
        session_id: acceptor_session_id,
        match_info: MatchInfo {
            role: PlayerRole::Join,
            peer_endpoint: challenger_stun,
            punch_at_ms,
            room_id: match_room_id,
            username: challenge.challenger_username,
            turn: join_creds,
        },
    }))
}

pub async fn decline_challenge(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(challenge_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    let challenge = state
        .challenges
        .remove(&challenge_id)
        .map(|(_, challenge)| challenge)
        .ok_or_else(|| AppError::NotFound("Challenge not found or expired".into()))?;

    if challenge.target_discord_id != claims.sub && challenge.challenger_discord_id != claims.sub {
        state
            .challenges
            .insert(challenge.challenge_id.clone(), challenge);
        return Err(AppError::Unauthorized(
            "Challenge does not belong to you".into(),
        ));
    }

    if let Some(mut entry) = state.queue.get_mut(&challenge.challenger_session_id) {
        entry.cancelled = true;
    }
    if let Some(sid) = state
        .player_sessions
        .get(&challenge.challenger_discord_id)
        .map(|sid| sid.clone())
    {
        if sid == challenge.challenger_session_id {
            state
                .player_sessions
                .remove(&challenge.challenger_discord_id);
        }
    }

    Ok(Json(json!({ "status": "declined" })))
}

fn touch_lobby_presence(state: &AppState, claims: &Claims, status: &str) {
    state.lobby_presence.insert(
        claims.sub.clone(),
        LobbyPresence {
            player_id: claims.sub.clone(),
            username: claims.username.clone(),
            status: status.to_string(),
            last_seen: Utc::now(),
        },
    );
}

fn general_lobby_snapshot(state: &AppState) -> GeneralLobbyResponse {
    let now = Utc::now();
    let mut users: Vec<LobbyUser> = state
        .lobby_presence
        .iter()
        .filter(|entry| (now - entry.last_seen).num_seconds() <= LOBBY_PRESENCE_VISIBLE_SECS)
        .map(|entry| LobbyUser {
            player_id: entry.player_id.clone(),
            username: entry.username.clone(),
            status: entry.status.clone(),
            rating: state.ratings.get(&entry.player_id).map(|r| r.0),
        })
        .collect();
    users.sort_by(|a, b| {
        a.username
            .cmp(&b.username)
            .then_with(|| a.player_id.cmp(&b.player_id))
    });

    let mut chat_entries: Vec<LobbyChatEntry> = state
        .lobby_chat
        .iter()
        .map(|entry| entry.value().clone())
        .collect();
    chat_entries.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.player_id.cmp(&b.player_id))
            .then_with(|| a.message_id.cmp(&b.message_id))
    });
    let skip = chat_entries.len().saturating_sub(LOBBY_CHAT_VISIBLE);
    let chat = chat_entries
        .into_iter()
        .skip(skip)
        .map(|entry| LobbyChatMessage {
            username: entry.username,
            message: entry.message,
            timestamp: entry.created_at.to_rfc3339(),
        })
        .collect();

    GeneralLobbyResponse {
        status: "General lobby ready".into(),
        users,
        chat,
    }
}

fn normalize_lobby_chat(raw: &str) -> Result<String, AppError> {
    let message = raw.trim();
    if message.is_empty() {
        return Err(AppError::BadRequest("Message is empty".into()));
    }
    if message.chars().count() > LOBBY_CHAT_MAX_CHARS {
        return Err(AppError::BadRequest("Message is too long".into()));
    }
    if message.chars().any(|c| c.is_control() && c != '\t') {
        return Err(AppError::BadRequest(
            "Message contains control characters".into(),
        ));
    }
    Ok(message.to_string())
}

fn lobby_room_summary(room: &SparRoom) -> LobbyRoomSummary {
    LobbyRoomSummary {
        id: room.room_id.clone(),
        name: room.name.clone(),
        host_username: room.creator_username.clone(),
        format: room.format.clone(),
        players: 1,
        private: room.private,
        status: "open".into(),
    }
}

fn challenge_summary(challenge: &Challenge, direction: &str) -> ChallengeSummary {
    ChallengeSummary {
        challenge_id: challenge.challenge_id.clone(),
        direction: direction.into(),
        challenger_id: challenge.challenger_discord_id.clone(),
        challenger_username: challenge.challenger_username.clone(),
        target_id: challenge.target_discord_id.clone(),
        target_username: challenge.target_username.clone(),
        format: challenge.format.clone(),
        created_at: challenge.created_at.to_rfc3339(),
    }
}

fn sanitize_lobby_name(name: Option<&str>, fallback: &str) -> String {
    let source = name.unwrap_or(fallback).trim();
    let mut out = String::new();
    for c in source.chars() {
        if c.is_ascii_alphanumeric() || c == ' ' || c == '_' || c == '-' {
            if c.is_whitespace() {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            } else {
                out.push(c);
            }
        }
        if out.len() >= 32 {
            break;
        }
    }
    let out = out.trim().to_string();
    if out.is_empty() {
        format!("{fallback}'s Lobby")
    } else {
        out
    }
}

// ── GET /match/status/:session_id ─────────────────────────────────────────────

pub async fn match_status(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(session_id): Path<String>,
) -> Result<Json<MatchStatusResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;

    let entry = state
        .queue
        .get(&session_id)
        .ok_or_else(|| AppError::NotFound("Session not found".into()))?;

    if entry.discord_id != claims.sub {
        return Err(AppError::Unauthorized(
            "Session does not belong to you".into(),
        ));
    }

    if entry.cancelled {
        return Ok(Json(MatchStatusResponse {
            status: MatchStatus::Cancelled,
            match_info: None,
        }));
    }

    let info = entry.match_info.clone();
    drop(entry);

    if let Some(info) = info {
        // Detect partner-side cancellation. Without this the client polls
        // "Matched" forever, then dies at hole-punch with a misleading
        // "peer didn't respond" error. Look up the partner by room_id and
        // surface their cancellation here.
        let partner_cancelled = state.queue.iter().any(|e| {
            e.discord_id != claims.sub
                && e.match_info.as_ref().map(|m| &m.room_id) == Some(&info.room_id)
                && e.cancelled
        });
        if partner_cancelled {
            tracing::info!(
                "[status] partner cancelled — reporting Cancelled to {} (sid={})",
                claims.username,
                session_id,
            );
            return Ok(Json(MatchStatusResponse {
                status: MatchStatus::Cancelled,
                match_info: None,
            }));
        }
        Ok(Json(MatchStatusResponse {
            status: MatchStatus::Matched,
            match_info: Some(info),
        }))
    } else {
        Ok(Json(MatchStatusResponse {
            status: MatchStatus::Queued,
            match_info: None,
        }))
    }
}

// ── POST /match/cancel ────────────────────────────────────────────────────────

pub async fn cancel_queue(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    if let Some(sid) = state.player_sessions.get(&claims.sub).map(|s| s.clone()) {
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

    let mut entry = state
        .queue
        .get_mut(&session_id)
        .ok_or_else(|| AppError::NotFound("Session not found".into()))?;

    if entry.discord_id != claims.sub {
        return Err(AppError::Unauthorized(
            "Session does not belong to you".into(),
        ));
    }

    entry.relayed_addr = Some(req.relayed_addr.clone());
    tracing::info!(
        "[turn-ready] {} relayed_addr={}",
        entry.username,
        req.relayed_addr
    );

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

    let entry = state
        .queue
        .get(&session_id)
        .ok_or_else(|| AppError::NotFound("Session not found".into()))?;

    if entry.discord_id != claims.sub {
        return Err(AppError::Unauthorized(
            "Session does not belong to you".into(),
        ));
    }

    let info = entry
        .match_info
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("Not matched yet".into()))?;
    let room_id = info.room_id.clone();
    drop(entry); // release the dashmap borrow before the next iter

    // Find the OTHER player in the same room
    let peer_relayed = state
        .queue
        .iter()
        .find(|e| {
            e.discord_id != claims.sub
                && e.match_info.as_ref().map(|m| &m.room_id) == Some(&room_id)
        })
        .and_then(|e| e.relayed_addr.clone());

    Ok(Json(PeerRelayResponse {
        peer_relayed_addr: peer_relayed,
    }))
}

// ── Signal stubs (kept for future ICE support) ───────────────────────────────

pub async fn signal_offer(
    State(_): State<AppState>,
    TypedHeader(_): TypedHeader<Authorization<Bearer>>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!({ "ok": true })))
}

pub async fn signal_answer(
    State(_): State<AppState>,
    TypedHeader(_): TypedHeader<Authorization<Bearer>>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!({ "ok": true })))
}

pub async fn signal_candidate(
    State(_): State<AppState>,
    TypedHeader(_): TypedHeader<Authorization<Bearer>>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!({ "ok": true })))
}

pub async fn signal_poll(
    State(_): State<AppState>,
    TypedHeader(_): TypedHeader<Authorization<Bearer>>,
    Path(_): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!({ "candidates": [] })))
}

// ── POST /match/result ─────────────────────────────────────────────────────
//
// Both clients report the same synced-GGRS outcome
// {session_id, match_index, p1_score, p2_score}.
// P1 in the game RAM = Host (local_handle 0), P2 = Join (local_handle 1).
// The server deduplicates by room_id + match_index, determines the winner,
// then forwards the result to the stats service for Glicko-2 rating update +
// leaderboard storage.

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

    let entry = state
        .queue
        .get(&req.session_id)
        .ok_or_else(|| AppError::NotFound("Session not found".into()))?;

    if entry.discord_id != claims.sub {
        return Err(AppError::Unauthorized(
            "Session does not belong to you".into(),
        ));
    }

    let info = entry
        .match_info
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("Match not yet started".into()))?;
    let room_id = info.room_id.clone();
    let match_index = req.match_index;
    let result_id = format!("{room_id}#{match_index}");
    let own_role = info.role.clone();
    let own_discord_id = entry.discord_id.clone();
    let own_username = entry.username.clone();
    let rom_hash = entry.rom_hash.clone();
    drop(entry);

    // Dedup: both clients post the same outcome (deterministic GGRS state).
    // The first poster lands in `pending_results`; the second one validates
    // and graduates the pair into `confirmed_results`. Without this check
    // either client can unilaterally report any score and farm rating.
    if state.confirmed_results.contains_key(&result_id) {
        return Ok(Json(json!({ "ok": true, "already_recorded": true })));
    }

    // Pending phase: have we seen the first half of this match's report?
    if let Some(pending) = state.pending_results.get(&result_id).map(|p| p.clone()) {
        if pending.reporter_discord_id == own_discord_id {
            // Same client reporting again. Idempotent — accept silently.
            return Ok(Json(json!({ "ok": true, "duplicate_self_report": true })));
        }
        if pending.p1_score != req.p1_score || pending.p2_score != req.p2_score {
            tracing::warn!(
                "[result] mismatch — result={} reporter1={} ({}-{}) reporter2={} ({}-{}). \
                 Rejecting both. Possible cheating attempt or genuine GGRS desync.",
                result_id,
                pending.reporter_discord_id,
                pending.p1_score,
                pending.p2_score,
                own_discord_id,
                req.p1_score,
                req.p2_score,
            );
            // Capture an incident before we drop the pending entry. The
            // bucket record is the only persistent trace once the in-memory
            // queue/pending state is reaped — without it the warn! above
            // is the entire investigation surface.
            record_server_incident(
                &state,
                ServerIncident {
                    kind: "score_mismatch".into(),
                    summary: format!(
                        "{} reported {}-{}; {} reported {}-{}",
                        pending.reporter_discord_id,
                        pending.p1_score,
                        pending.p2_score,
                        own_discord_id,
                        req.p1_score,
                        req.p2_score,
                    ),
                    room_id: room_id.clone(),
                    session_ids: vec![req.session_id.clone()],
                    usernames: vec![own_username.clone()],
                    details: serde_json::json!({
                        "result_id": result_id,
                        "match_index": match_index,
                        "first_reporter_discord_id": pending.reporter_discord_id,
                        "first_reporter_p1": pending.p1_score,
                        "first_reporter_p2": pending.p2_score,
                        "second_reporter_discord_id": own_discord_id,
                        "second_reporter_p1": req.p1_score,
                        "second_reporter_p2": req.p2_score,
                        "first_reported_at": pending.reported_at.to_rfc3339(),
                        "rom_hash": rom_hash,
                    }),
                },
            );
            // Drop the pending entry. Whoever reports next is treated as a
            // first-time reporter — but with a logged-cheat-attempt trail.
            state.pending_results.remove(&result_id);
            return Err(AppError::BadRequest(
                "Score mismatch with opponent's report — match not committed".into(),
            ));
        }
        // Both halves agree. Promote to confirmed and continue to forward.
        state.pending_results.remove(&result_id);
        state.confirmed_results.insert(
            result_id.clone(),
            ConfirmedResult {
                committed_at: Utc::now(),
            },
        );
    } else {
        // First report — stash and wait for the partner. The TTL sweeper
        // will remove this if the partner never reports.
        state.pending_results.insert(
            result_id.clone(),
            PendingResult {
                reporter_discord_id: own_discord_id.clone(),
                p1_score: req.p1_score,
                p2_score: req.p2_score,
                reported_at: Utc::now(),
            },
        );
        tracing::debug!(
            "[result] pending — result={} reporter={} ({}-{}). Awaiting partner.",
            result_id,
            own_discord_id,
            req.p1_score,
            req.p2_score,
        );
        return Ok(Json(
            json!({ "ok": true, "pending_partner_confirmation": true }),
        ));
    }

    // Find the opponent's entry in the same room (for usernames + roles).
    let (opp_discord_id, opp_role, opp_username) = state
        .queue
        .iter()
        .find(|e| {
            e.discord_id != own_discord_id
                && e.match_info.as_ref().map(|m| &m.room_id) == Some(&room_id)
        })
        .map(|e| {
            (
                e.discord_id.clone(),
                e.match_info.as_ref().map(|m| m.role.clone()),
                e.username.clone(),
            )
        })
        .ok_or_else(|| AppError::BadRequest("Opponent not found in queue".into()))?;

    let opp_role =
        opp_role.ok_or_else(|| AppError::BadRequest("Opponent match info missing".into()))?;

    // Determine winner. Host = P1 in RAM (local_handle 0), Join = P2.
    let (host_id, join_id) = match (&own_role, &opp_role) {
        (PlayerRole::Host, PlayerRole::Join) => (own_discord_id.clone(), opp_discord_id.clone()),
        (PlayerRole::Join, PlayerRole::Host) => (opp_discord_id.clone(), own_discord_id.clone()),
        _ => return Err(AppError::BadRequest("Unexpected role pair".into())),
    };

    let host_won = req.p1_score > req.p2_score;

    let (winner_id, loser_id, winner_score, loser_score, winner_username, loser_username) =
        if host_won {
            let w_username = if own_role == PlayerRole::Host {
                own_username.clone()
            } else {
                opp_username.clone()
            };
            let l_username = if own_role == PlayerRole::Host {
                opp_username.clone()
            } else {
                own_username.clone()
            };
            (
                host_id.clone(),
                join_id.clone(),
                req.p1_score,
                req.p2_score,
                w_username,
                l_username,
            )
        } else {
            let w_username = if own_role == PlayerRole::Join {
                own_username.clone()
            } else {
                opp_username.clone()
            };
            let l_username = if own_role == PlayerRole::Join {
                opp_username.clone()
            } else {
                own_username.clone()
            };
            (
                join_id.clone(),
                host_id.clone(),
                req.p2_score,
                req.p1_score,
                w_username,
                l_username,
            )
        };

    tracing::info!(
        "Match result: {} beat {} {}:{} (room={} match_index={})",
        winner_id,
        loser_id,
        winner_score,
        loser_score,
        room_id,
        match_index
    );

    let koh_winner = winner_id.clone();
    let koh_loser = loser_id.clone();
    // Unranked king-of-the-hill matches are casual — don't touch ratings.
    let skip_stats = is_unranked_lobby_room(&state, &room_id);

    // Forward to stats service if configured. Retry with backoff on transient
    // failure so a cold-starting stats Cloud Run instance doesn't drop the
    // result. Auth errors / 4xx aren't retried — those are configuration bugs.
    if !skip_stats {
        if let (Some(url), Some(key)) =
            (&state.config.stats_service_url, &state.config.stats_api_key)
        {
            let payload = StatsForward {
                room_id: result_id,
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
                forward_to_stats_with_retry(&state_clone, &url_clone, &key_clone, &payload).await;
            });
        }
    }

    // If this was a king-of-the-hill lobby match, rotate the queue — but only
    // once the whole set is decided, not after each game of a best-of-N.
    if req.set_over {
        koh_on_result(&state, &room_id, &koh_winner, &koh_loser);
    }

    Ok(Json(json!({ "ok": true })))
}

async fn forward_to_stats_with_retry(
    state: &AppState,
    url: &str,
    api_key: &str,
    payload: &StatsForward,
) {
    // 5 attempts: 1s, 3s, 8s, 20s, 50s. Total ~80s, comfortably inside the
    // confirmed_results TTL (10 min) so a duplicate report mid-retry is still
    // caught by the dedup path.
    const BACKOFFS_SECS: [u64; 4] = [1, 3, 8, 20];

    for attempt in 0..5usize {
        match state
            .http
            .post(url)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(payload)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    tracing::debug!("Forwarded match result to stats (attempt {})", attempt + 1);
                    return;
                }
                if status.is_client_error() {
                    tracing::error!(
                        "Stats service rejected match (room={}): {} — not retrying (4xx)",
                        payload.room_id,
                        status,
                    );
                    return;
                }
                tracing::warn!(
                    "Stats service returned {} (attempt {}/5, room={})",
                    status,
                    attempt + 1,
                    payload.room_id,
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Stats forward error (attempt {}/5, room={}): {e}",
                    attempt + 1,
                    payload.room_id,
                );
            }
        }
        if let Some(delay) = BACKOFFS_SECS.get(attempt) {
            tokio::time::sleep(std::time::Duration::from_secs(*delay)).await;
        }
    }
    tracing::error!(
        "Match result lost — gave up after 5 attempts (room={}). \
         Consider persisting to a durable retry queue.",
        payload.room_id,
    );
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
    let lobby_name = sanitize_lobby_name(req.name.as_deref(), &claims.username);

    // Register the room so someone can join it
    state.spar_rooms.insert(
        room_id.clone(),
        SparRoom {
            room_id: room_id.clone(),
            creator_discord_id: claims.sub.clone(),
            creator_username: claims.username.clone(),
            created_at: Utc::now(),
            name: lobby_name,
            format: req.format.clone(),
            private: req.private,
            app_version: req.app_version.clone(),
            rom_hash: req.rom_hash.clone(),
        },
    );

    // Create a placeholder queue entry so the creator can poll /match/status
    state.queue.insert(
        creator_session_id.clone(),
        QueueEntry {
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
        },
    );

    state
        .player_sessions
        .insert(claims.sub.clone(), creator_session_id.clone());

    tracing::info!("Room {} created by {}", room_id, claims.username);
    Ok(Json(CreateRoomResponse {
        room_id,
        creator_session_id,
    }))
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
    let room = state
        .spar_rooms
        .remove(&room_id)
        .map(|(_, r)| r)
        .ok_or_else(|| AppError::NotFound("Room not found or already joined".into()))?;

    if room.creator_discord_id == claims.sub {
        state.spar_rooms.insert(room_id, room);
        return Err(AppError::BadRequest("Cannot join your own room".into()));
    }

    if room.app_version != req.app_version || room.rom_hash != req.rom_hash {
        state.spar_rooms.insert(room_id, room);
        return Err(AppError::BadRequest(
            "Room requires matching app version and ROM hash".into(),
        ));
    }

    // Find the creator's queue entry
    let creator_session_id = state
        .player_sessions
        .get(&room.creator_discord_id)
        .map(|s| s.clone())
        .ok_or_else(|| AppError::NotFound("Room creator is no longer available".into()))?;

    let Some(mut creator_entry) = state.queue.get_mut(&creator_session_id) else {
        return Err(AppError::NotFound("Room creator session expired".into()));
    };
    if creator_entry.cancelled || creator_entry.match_info.is_some() {
        return Err(AppError::NotFound(
            "Room creator is no longer in queue".into(),
        ));
    }

    let joiner_session_id = Uuid::new_v4().to_string();
    let match_room_id = Uuid::new_v4().to_string();
    let punch_at_ms = (Utc::now() + chrono::Duration::milliseconds(2500)).timestamp_millis();
    let host_creds = mint_turn_for_room(&state, &match_room_id, 0);
    let join_creds = mint_turn_for_room(&state, &match_room_id, 1);

    // Copy the creator's STUN endpoint before we overwrite their entry
    let creator_stun = creator_entry.stun_endpoint.clone();

    // Update creator's entry: they are Host, seeing the joiner as peer
    creator_entry.match_info = Some(MatchInfo {
        role: PlayerRole::Host,
        peer_endpoint: req.stun_endpoint.clone(),
        punch_at_ms,
        room_id: match_room_id.clone(),
        username: claims.username.clone(),
        turn: host_creds,
    });
    drop(creator_entry);

    // Create joiner's entry: they are Join
    state.queue.insert(
        joiner_session_id.clone(),
        QueueEntry {
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
                turn: join_creds.clone(),
            }),
            cancelled: false,
            relayed_addr: None,
        },
    );

    state
        .player_sessions
        .insert(claims.sub.clone(), joiner_session_id.clone());

    let (s, u1, u2) = (
        state.clone(),
        room.creator_username.clone(),
        claims.username.clone(),
    );
    tokio::spawn(async move { discord::notify_matched(&s, &u1, &u2).await });

    tracing::info!(
        "Room {} joined: {} (creator) vs {} (joiner)",
        room_id,
        room.creator_username,
        claims.username
    );

    Ok(Json(JoinRoomResponse {
        session_id: joiner_session_id,
        match_info: MatchInfo {
            role: PlayerRole::Join,
            peer_endpoint: creator_stun,
            punch_at_ms,
            room_id: match_room_id,
            username: room.creator_username.clone(),
            turn: join_creds,
        },
    }))
}

// ── King-of-the-hill lobbies ────────────────────────────────────────────────

/// How long the incoming challenger has to confirm they're ready.
const READY_TIMEOUT_SECS: i64 = 10;

/// Pick the next champion + challenger from the queue and open a ready check.
/// The champion (front of the queue: the winner staying on the hill, or the
/// lobby's first player) is auto-ready; the challenger must confirm within
/// READY_TIMEOUT_SECS or be dropped to spectating.
fn advance_lobby(_state: &AppState, lobby: &mut KohLobby) {
    if lobby.current.is_some() || lobby.pending.is_some() {
        return;
    }
    // Drop any queued ids that are no longer members.
    let member_ids: std::collections::HashSet<String> =
        lobby.members.iter().map(|m| m.player_id.clone()).collect();
    lobby.queue.retain(|id| member_ids.contains(id));
    if lobby.queue.len() < 2 {
        return;
    }
    let champion = lobby.queue.pop_front().unwrap();
    let challenger = lobby.queue.pop_front().unwrap();
    tracing::info!("Lobby {} ready-check: champion vs challenger", lobby.id);
    lobby.pending = Some(PendingMatch {
        champion,
        challenger,
        deadline: Utc::now() + chrono::Duration::seconds(READY_TIMEOUT_SECS),
    });
    lobby.last_activity = Utc::now();
}

/// If the ready check timed out, drop the challenger to spectating, return the
/// champion to the front of the queue, and re-advance to the next challenger.
fn check_pending(state: &AppState, lobby: &mut KohLobby) {
    let expired = lobby
        .pending
        .as_ref()
        .map_or(false, |p| Utc::now() > p.deadline);
    if !expired {
        return;
    }
    let p = lobby.pending.take().unwrap();
    if lobby.members.iter().any(|m| m.player_id == p.champion)
        && !lobby.queue.iter().any(|id| id == &p.champion)
    {
        lobby.queue.push_front(p.champion);
    }
    lobby.queue.retain(|id| id != &p.challenger);
    if let Some(m) = lobby.members.iter_mut().find(|m| m.player_id == p.challenger) {
        m.role = LobbyRole::Spectating; // dropped from the rotation; can re-queue
    }
    tracing::info!("Lobby {} ready-check timed out — challenger skipped", lobby.id);
    lobby.last_activity = Utc::now();
    advance_lobby(state, lobby);
}

/// Both parties are go — start the actual match. Inserts queue entries for the
/// two sessions so the existing /match/status, /turn-ready and /match/result
/// machinery applies unchanged.
fn promote_pending(state: &AppState, lobby: &mut KohLobby) {
    let Some(p) = lobby.pending.take() else {
        return;
    };
    let host_m = lobby.members.iter().find(|m| m.player_id == p.champion).cloned();
    let join_m = lobby
        .members
        .iter()
        .find(|m| m.player_id == p.challenger)
        .cloned();
    let (Some(host_m), Some(join_m)) = (host_m, join_m) else {
        for id in [p.champion, p.challenger] {
            if lobby.members.iter().any(|m| m.player_id == id) {
                lobby.queue.push_front(id);
            }
        }
        advance_lobby(state, lobby);
        return;
    };

    let room_id = Uuid::new_v4().to_string();
    let host_session = Uuid::new_v4().to_string();
    let join_session = Uuid::new_v4().to_string();
    let punch_at_ms = (Utc::now() + chrono::Duration::milliseconds(2500)).timestamp_millis();
    let host_creds = mint_turn_for_room(state, &room_id, 0);
    let join_creds = mint_turn_for_room(state, &room_id, 1);

    let mk_entry = |session: &str, me: &LobbyMember, role: PlayerRole, peer: &LobbyMember, turn| {
        QueueEntry {
            session_id: session.to_string(),
            discord_id: me.player_id.clone(),
            username: me.username.clone(),
            stun_endpoint: me.stun_endpoint.clone(),
            app_version: lobby.app_version.clone(),
            rom_hash: lobby.rom_hash.clone(),
            queued_at: Utc::now(),
            match_info: Some(MatchInfo {
                role,
                peer_endpoint: peer.stun_endpoint.clone(),
                punch_at_ms,
                room_id: room_id.clone(),
                username: peer.username.clone(),
                turn,
            }),
            cancelled: false,
            relayed_addr: None,
        }
    };
    state.queue.insert(
        host_session.clone(),
        mk_entry(&host_session, &host_m, PlayerRole::Host, &join_m, host_creds),
    );
    state.queue.insert(
        join_session.clone(),
        mk_entry(&join_session, &join_m, PlayerRole::Join, &host_m, join_creds),
    );
    state
        .player_sessions
        .insert(host_m.player_id.clone(), host_session.clone());
    state
        .player_sessions
        .insert(join_m.player_id.clone(), join_session.clone());

    tracing::info!(
        "Lobby {} pairing {} vs {}",
        lobby.id,
        host_m.username,
        join_m.username
    );
    lobby.current = Some(ActiveMatch {
        host: host_m.player_id,
        join: join_m.player_id,
        room_id,
        host_session,
        join_session,
        started_at: Utc::now(),
    });
    lobby.last_activity = Utc::now();
}

/// Build the caller's view of a lobby.
fn lobby_state_for(state: &AppState, lobby: &KohLobby, caller: &str) -> LobbyStateResponse {
    let in_match = |pid: &str| {
        lobby
            .current
            .as_ref()
            .map_or(false, |c| c.host == pid || c.join == pid)
    };
    let username_of = |pid: &str| {
        lobby
            .members
            .iter()
            .find(|m| m.player_id == pid)
            .map(|m| m.username.clone())
            .unwrap_or_default()
    };
    let members = lobby
        .members
        .iter()
        .map(|m| LobbyMemberView {
            player_id: m.player_id.clone(),
            username: m.username.clone(),
            rating: state.ratings.get(&m.player_id).map(|r| r.0),
            role: m.role,
            in_match: in_match(&m.player_id),
        })
        .collect();
    let queue = lobby.queue.iter().map(|id| username_of(id)).collect();
    let current = lobby.current.as_ref().map(|c| CurrentMatchView {
        host_username: username_of(&c.host),
        join_username: username_of(&c.join),
        host_session: c.host_session.clone(),
        join_session: c.join_session.clone(),
    });
    let ready_check = lobby.pending.as_ref().map(|p| ReadyCheckView {
        champion_username: username_of(&p.champion),
        challenger_username: username_of(&p.challenger),
        seconds_left: (p.deadline - Utc::now()).num_seconds().max(0),
        you_are_challenger: p.challenger == caller,
    });
    let your_session = lobby.current.as_ref().and_then(|c| {
        if c.host == caller {
            Some(c.host_session.clone())
        } else if c.join == caller {
            Some(c.join_session.clone())
        } else {
            None
        }
    });
    let your_match = your_session
        .as_ref()
        .and_then(|s| state.queue.get(s).and_then(|e| e.match_info.clone()));
    LobbyStateResponse {
        id: lobby.id.clone(),
        name: lobby.name.clone(),
        ranked: lobby.ranked,
        private: lobby.private,
        format: lobby.format.clone(),
        members,
        queue,
        current,
        ready_check,
        your_position: lobby.queue.iter().position(|id| id == caller),
        your_role: lobby
            .members
            .iter()
            .find(|m| m.player_id == caller)
            .map(|m| m.role),
        your_match,
        your_session,
    }
}

/// Remove a member; if they were mid-match the opponent wins by default. Caller
/// must destroy the lobby afterward if `members` is now empty.
fn remove_lobby_member(state: &AppState, lobby: &mut KohLobby, player_id: &str) {
    lobby.queue.retain(|id| id != player_id);
    lobby.members.retain(|m| m.player_id != player_id);
    if let Some(c) = lobby.current.clone() {
        if c.host == player_id || c.join == player_id {
            let winner = if c.host == player_id { c.join } else { c.host };
            state.queue.remove(&c.host_session);
            state.queue.remove(&c.join_session);
            lobby.current = None;
            if lobby.members.iter().any(|m| m.player_id == winner)
                && !lobby.queue.iter().any(|id| id == &winner)
            {
                lobby.queue.push_front(winner);
            }
        }
    }
    // If they were in a pending ready check, cancel it and re-queue the other.
    if let Some(p) = lobby.pending.clone() {
        if p.champion == player_id || p.challenger == player_id {
            lobby.pending = None;
            let other = if p.champion == player_id {
                p.challenger
            } else {
                p.champion
            };
            if lobby.members.iter().any(|m| m.player_id == other)
                && !lobby.queue.iter().any(|id| id == &other)
            {
                lobby.queue.push_front(other);
            }
        }
    }
    lobby.last_activity = Utc::now();
    advance_lobby(state, lobby);
}

/// Hook from /match/result: advance the lobby whose current match just ended.
/// Winner returns to the front of the queue, loser to the back.
pub fn koh_on_result(state: &AppState, room_id: &str, winner_id: &str, loser_id: &str) {
    let lobby_id = state
        .koh_lobbies
        .iter()
        .find(|e| {
            e.value()
                .current
                .as_ref()
                .map_or(false, |c| c.room_id == room_id)
        })
        .map(|e| e.key().clone());
    let Some(lobby_id) = lobby_id else { return };
    if let Some(mut lobby) = state.koh_lobbies.get_mut(&lobby_id) {
        if let Some(c) = lobby.current.clone() {
            state.queue.remove(&c.host_session);
            state.queue.remove(&c.join_session);
        }
        lobby.current = None;
        if lobby.members.iter().any(|m| m.player_id == winner_id)
            && !lobby.queue.iter().any(|id| id == winner_id)
        {
            lobby.queue.push_front(winner_id.to_string());
        }
        if lobby.members.iter().any(|m| m.player_id == loser_id)
            && !lobby.queue.iter().any(|id| id == loser_id)
        {
            lobby.queue.push_back(loser_id.to_string());
        }
        lobby.last_activity = Utc::now();
        advance_lobby(&state, &mut lobby);
        tracing::info!(
            "Lobby {} advanced: {} stays, {} requeued",
            lobby_id,
            winner_id,
            loser_id
        );
    }
}

/// True if `room_id` is the active match of an *unranked* KoH lobby (skip stats).
pub fn is_unranked_lobby_room(state: &AppState, room_id: &str) -> bool {
    state.koh_lobbies.iter().any(|e| {
        !e.value().ranked
            && e.value()
                .current
                .as_ref()
                .map_or(false, |c| c.room_id == room_id)
    })
}

pub async fn create_lobby(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Json(req): Json<CreateLobbyRequest>,
) -> Result<Json<CreateLobbyResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    if req.stun_endpoint.is_empty() || !req.stun_endpoint.contains(':') {
        return Err(AppError::BadRequest("Invalid stun_endpoint".into()));
    }
    // Short, typeable id so it doubles as an invite code for private lobbies.
    let lobby_id = loop {
        let code = Uuid::new_v4().simple().to_string()[..6].to_uppercase();
        if !state.koh_lobbies.contains_key(&code) {
            break code;
        }
    };
    let name = sanitize_lobby_name(Some(&req.name), &claims.username);
    let lobby = KohLobby {
        id: lobby_id.clone(),
        name,
        ranked: req.ranked,
        private: req.private,
        format: req.format,
        host_id: claims.sub.clone(),
        app_version: req.app_version,
        rom_hash: req.rom_hash,
        created_at: Utc::now(),
        last_activity: Utc::now(),
        members: vec![LobbyMember {
            player_id: claims.sub.clone(),
            username: claims.username.clone(),
            stun_endpoint: req.stun_endpoint,
            role: LobbyRole::Queued,
            last_seen: Utc::now(),
        }],
        queue: std::collections::VecDeque::from([claims.sub.clone()]),
        pending: None,
        current: None,
    };
    state.koh_lobbies.insert(lobby_id.clone(), lobby);
    tracing::info!("Lobby {} created by {}", lobby_id, claims.username);
    Ok(Json(CreateLobbyResponse { lobby_id }))
}

pub async fn join_lobby(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(lobby_id): Path<String>,
    Json(req): Json<JoinLobbyRequest>,
) -> Result<Json<LobbyStateResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    if req.stun_endpoint.is_empty() || !req.stun_endpoint.contains(':') {
        return Err(AppError::BadRequest("Invalid stun_endpoint".into()));
    }
    let mut lobby = state
        .koh_lobbies
        .get_mut(&lobby_id)
        .ok_or_else(|| AppError::NotFound("Lobby not found or already closed".into()))?;
    if lobby.app_version != req.app_version || lobby.rom_hash != req.rom_hash {
        return Err(AppError::BadRequest(
            "Lobby requires matching app version and ROM hash".into(),
        ));
    }
    let role = if req.spectate {
        LobbyRole::Spectating
    } else {
        LobbyRole::Queued
    };
    if let Some(m) = lobby.members.iter_mut().find(|m| m.player_id == claims.sub) {
        m.stun_endpoint = req.stun_endpoint;
        m.role = role;
        m.last_seen = Utc::now();
    } else {
        lobby.members.push(LobbyMember {
            player_id: claims.sub.clone(),
            username: claims.username.clone(),
            stun_endpoint: req.stun_endpoint,
            role,
            last_seen: Utc::now(),
        });
    }
    let playing = lobby
        .current
        .as_ref()
        .map_or(false, |c| c.host == claims.sub || c.join == claims.sub);
    if role == LobbyRole::Queued {
        if !playing && !lobby.queue.iter().any(|id| id == &claims.sub) {
            lobby.queue.push_back(claims.sub.clone());
        }
    } else {
        lobby.queue.retain(|id| id != &claims.sub);
    }
    lobby.last_activity = Utc::now();
    advance_lobby(&state, &mut lobby);
    Ok(Json(lobby_state_for(&state, &lobby, &claims.sub)))
}

pub async fn get_lobby(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(lobby_id): Path<String>,
) -> Result<Json<LobbyStateResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    let mut lobby = state
        .koh_lobbies
        .get_mut(&lobby_id)
        .ok_or_else(|| AppError::NotFound("Lobby not found or already closed".into()))?;
    if let Some(m) = lobby.members.iter_mut().find(|m| m.player_id == claims.sub) {
        m.last_seen = Utc::now();
    }
    check_pending(&state, &mut lobby);
    advance_lobby(&state, &mut lobby);
    Ok(Json(lobby_state_for(&state, &lobby, &claims.sub)))
}

/// Challenger confirms they are ready for the pending match. When the front-of-
/// queue challenger readies within the deadline, we promote the pending pair
/// into a live match. The champion (winner staying / first player) is
/// auto-ready, so only the challenger needs to call this.
pub async fn ready_lobby(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(lobby_id): Path<String>,
) -> Result<Json<LobbyStateResponse>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    let mut lobby = state
        .koh_lobbies
        .get_mut(&lobby_id)
        .ok_or_else(|| AppError::NotFound("Lobby not found or already closed".into()))?;
    if let Some(m) = lobby.members.iter_mut().find(|m| m.player_id == claims.sub) {
        m.last_seen = Utc::now();
    }
    check_pending(&state, &mut lobby);
    let is_challenger = lobby
        .pending
        .as_ref()
        .map_or(false, |p| p.challenger == claims.sub);
    if is_challenger {
        promote_pending(&state, &mut lobby);
    }
    advance_lobby(&state, &mut lobby);
    Ok(Json(lobby_state_for(&state, &lobby, &claims.sub)))
}

pub async fn leave_lobby(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(lobby_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    let mut empty = false;
    if let Some(mut lobby) = state.koh_lobbies.get_mut(&lobby_id) {
        remove_lobby_member(&state, &mut lobby, &claims.sub);
        empty = lobby.members.is_empty();
    }
    if empty {
        state.koh_lobbies.remove(&lobby_id);
        state.lobby_thumbs.remove(&lobby_id);
        tracing::info!("Lobby {} destroyed (empty)", lobby_id);
    }
    Ok(Json(json!({ "status": "left" })))
}

pub async fn list_koh_lobbies(
    State(state): State<AppState>,
) -> Result<Json<LobbyListResponse>, AppError> {
    let mut lobbies: Vec<LobbyRoomSummary> = state
        .koh_lobbies
        .iter()
        .filter(|e| !e.value().private)
        .map(|e| {
            let l = e.value();
            let host = l
                .members
                .iter()
                .find(|m| m.player_id == l.host_id)
                .or_else(|| l.members.first())
                .map(|m| m.username.clone())
                .unwrap_or_else(|| "Host".into());
            LobbyRoomSummary {
                id: l.id.clone(),
                name: l.name.clone(),
                host_username: host,
                format: l.format.clone(),
                players: l.members.len().min(u8::MAX as usize) as u8,
                private: false,
                status: if l.current.is_some() {
                    "in match".into()
                } else {
                    "open".into()
                },
            }
        })
        .collect();
    lobbies.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    Ok(Json(LobbyListResponse { lobbies }))
}

/// Cap on a pushed lobby thumbnail. The client sends a small gzipped frame
/// (~tens of KB); this keeps the endpoint from being used as cheap storage.
const MAX_LOBBY_THUMB_BYTES: usize = 256 * 1024;

/// POST /koh/:lobby_id/thumb — an active player in the lobby's current match
/// pushes a periodic screenshot (opaque gzipped bytes). Only the two players in
/// the current match may push; everyone else just watches.
pub async fn put_lobby_thumb(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(lobby_id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    if body.len() > MAX_LOBBY_THUMB_BYTES {
        return Err(AppError::BadRequest("Thumbnail too large".into()));
    }
    let is_player = state.koh_lobbies.get(&lobby_id).map_or(false, |l| {
        l.current
            .as_ref()
            .map_or(false, |c| c.host == claims.sub || c.join == claims.sub)
    });
    if !is_player {
        return Err(AppError::Unauthorized(
            "Only active players can push a lobby thumbnail".into(),
        ));
    }
    state
        .lobby_thumbs
        .insert(lobby_id, (body.to_vec(), Utc::now()));
    Ok(Json(json!({ "ok": true })))
}

/// GET /koh/:lobby_id/thumb — latest match screenshot for the lobby. Public so
/// any spectator can show it. Returns the opaque (gzipped) bytes as-is.
pub async fn get_lobby_thumb(
    State(state): State<AppState>,
    Path(lobby_id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match state.lobby_thumbs.get(&lobby_id) {
        Some(entry) => {
            let bytes = entry.value().0.clone();
            ([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], bytes)
                .into_response()
        }
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

/// Member heartbeat lapse (clients poll GET /koh/:id every ~2s while viewing).
const LOBBY_MEMBER_TTL_SECS: i64 = 60;
/// A `current` match that never produced a result is abandoned after this.
const LOBBY_MATCH_TTL_SECS: i64 = 10 * 60;

/// Reap idle members, abandon stuck matches, and destroy empty lobbies.
/// Returns the number of lobbies destroyed. Active (playing) members are never
/// reaped for idleness — they aren't polling while in a match.
pub fn sweep_lobbies(state: &AppState) -> usize {
    let now = Utc::now();
    let ids: Vec<String> = state.koh_lobbies.iter().map(|e| e.key().clone()).collect();
    let mut destroyed = 0;
    for id in ids {
        let mut empty = false;
        if let Some(mut lobby) = state.koh_lobbies.get_mut(&id) {
            if let Some(c) = lobby.current.clone() {
                if (now - c.started_at).num_seconds() > LOBBY_MATCH_TTL_SECS {
                    state.queue.remove(&c.host_session);
                    state.queue.remove(&c.join_session);
                    lobby.current = None;
                }
            }
            let active: Vec<String> = lobby
                .current
                .as_ref()
                .map(|c| vec![c.host.clone(), c.join.clone()])
                .unwrap_or_default();
            let idle: Vec<String> = lobby
                .members
                .iter()
                .filter(|m| {
                    !active.contains(&m.player_id)
                        && (now - m.last_seen).num_seconds() > LOBBY_MEMBER_TTL_SECS
                })
                .map(|m| m.player_id.clone())
                .collect();
            for pid in idle {
                remove_lobby_member(state, &mut lobby, &pid);
            }
            // Expire stale ready-checks: a challenger who never confirmed in
            // time is dropped to spectating and the next player is pulled up.
            check_pending(state, &mut lobby);
            advance_lobby(state, &mut lobby);
            empty = lobby.members.is_empty();
        }
        if empty {
            state.koh_lobbies.remove(&id);
            state.lobby_thumbs.remove(&id);
            destroyed += 1;
        }
    }
    destroyed
}

// ── Spectator relay ────────────────────────────────────────────────────────

/// Playing peer pushes their latest confirmed frame so spectators can
/// reconstruct the view. Called periodically during the match.
///
/// Authenticated: only the player whose session_id this is may push.
/// Without this, anyone could overwrite live spectator frames for any
/// active match, garbaging the dashboard or impersonating players.
pub async fn spectator_push(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
    Path(session_id): Path<String>,
    Json(req): Json<SpectatorPushRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;

    // Verify the caller owns this session_id before they can push frames
    // for it. We do this here rather than at route level so the lookup is
    // close to the use; a single read of the queue entry is enough.
    {
        let entry = state
            .queue
            .get(&session_id)
            .ok_or_else(|| AppError::NotFound("Session not found".into()))?;
        if entry.discord_id != claims.sub {
            return Err(AppError::Unauthorized(
                "Session does not belong to you".into(),
            ));
        }
    }

    // Read player names from the queue entry
    let (p1_username, p2_username) = if let Some(entry) = state.queue.get(&session_id) {
        let p1 = entry.username.clone();
        let own_discord_id = entry.discord_id.clone();
        // Find opponent's entry sharing the same room_id.
        let room_id = entry
            .match_info
            .as_ref()
            .map(|m| m.room_id.clone())
            .unwrap_or_default();
        drop(entry);
        let p2 = state
            .queue
            .iter()
            .find(|e| {
                e.match_info
                    .as_ref()
                    .map(|m| m.room_id == room_id)
                    .unwrap_or(false)
                    && e.discord_id != own_discord_id
            })
            .map(|e| e.username.clone())
            .unwrap_or_else(|| "Opponent".to_string());
        (p1, p2)
    } else {
        ("Player 1".to_string(), "Player 2".to_string())
    };

    state.spectator_frames.insert(
        session_id.clone(),
        SpectatorFrame {
            savestate: req.savestate,
            inputs: req.inputs,
            frame: req.frame,
            score_p1: req.score_p1,
            score_p2: req.score_p2,
            p1_username,
            p2_username,
            updated_at: Utc::now(),
        },
    );

    tracing::debug!(
        "Spectator frame pushed for session {session_id}: frame={}",
        req.frame
    );
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
        let (sid, q) = entry.pair();
        if q.cancelled {
            continue;
        }
        let info = if let Some(ref info) = q.match_info {
            info
        } else {
            continue;
        };

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

        // spectator_frames is keyed by session_id (whichever side is
        // pushing), not by room_id. Check this side's first; if neither
        // side has pushed yet, the other side will when its iter pass
        // dedup'd by room_id picks it up.
        let (score_p1, score_p2) = state
            .spectator_frames
            .get(sid)
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

    // Deduplicate by room_id, preferring the entry that has a non-zero
    // score (i.e. the side that actually pushed a spectator frame).
    matches.sort_by(|a, b| {
        let a_active = (a.score_p1 + a.score_p2) > 0;
        let b_active = (b.score_p1 + b.score_p2) > 0;
        b_active.cmp(&a_active)
    });

    // Deduplicate by room_id (both queue entries share the same room_id)
    let mut seen = std::collections::HashSet::new();
    matches.retain(|m| seen.insert(m.room_id.clone()));

    Ok(Json(LiveMatchesResponse { matches }))
}

#[cfg(test)]
mod tests {
    use super::{lobby_room_summary, normalize_lobby_chat, sanitize_lobby_name};
    use crate::models::{LobbyMatchFormat, SparRoom};
    use chrono::Utc;

    #[test]
    fn sanitize_lobby_name_keeps_plain_player_text() {
        assert_eq!(
            sanitize_lobby_name(Some("  Friday FT5!!!  "), "Kitana"),
            "Friday FT5"
        );
        assert_eq!(sanitize_lobby_name(Some("!!!"), "Kitana"), "Kitana's Lobby");
        assert_eq!(sanitize_lobby_name(None, "SubZero"), "SubZero");
    }

    #[test]
    fn normalize_lobby_chat_trims_and_rejects_bad_messages() {
        assert_eq!(normalize_lobby_chat("  ft5?  ").unwrap(), "ft5?");
        assert!(normalize_lobby_chat("").is_err());
        assert!(normalize_lobby_chat(&"x".repeat(181)).is_err());
        assert!(normalize_lobby_chat("hello\u{0007}").is_err());
    }

    #[test]
    fn lobby_room_summary_keeps_public_lobby_metadata() {
        let room = SparRoom {
            room_id: "room-1".into(),
            creator_discord_id: "p1".into(),
            creator_username: "Jax".into(),
            created_at: Utc::now(),
            name: "Long Sets".into(),
            format: LobbyMatchFormat::Ft10,
            private: false,
            app_version: "0.7.10".into(),
            rom_hash: "abcd".into(),
        };

        let summary = lobby_room_summary(&room);

        assert_eq!(summary.id, "room-1");
        assert_eq!(summary.name, "Long Sets");
        assert_eq!(summary.host_username, "Jax");
        assert_eq!(summary.format, LobbyMatchFormat::Ft10);
        assert_eq!(summary.players, 1);
        assert!(!summary.private);
        assert_eq!(summary.status, "open");
    }
}
