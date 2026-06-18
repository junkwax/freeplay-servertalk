//! Background reaper for matchmaking state.
//!
//! Cloud Run instances do reset eventually, but until they do every
//! abandoned queue/spar/spectator entry is permanent in-process garbage.
//! On a busy day that's an OOM waiting to happen. This task wakes up
//! periodically and removes anything older than its category's TTL.
//!
//! TTLs are tuned for the "what would a human player ever care about"
//! horizon, with one extra buffer on top for clock skew between the
//! signaling server and the clients.

use chrono::Utc;
use std::time::Duration;

use crate::incidents::{record_server_incident, ServerIncident};
use crate::state::AppState;

/// Queue entries (lfg, status polling, post-match holdover) older than this
/// are reaped. Two minutes matches the client's `poll_status` deadline plus
/// some slack for clients that disconnected after being matched.
const QUEUE_TTL_SECS: i64 = 5 * 60;

/// Spar rooms older than 10 minutes get cleaned up. The Discord RPC join
/// secret stays advertised on the inviter's profile; if no one clicks it
/// in 10 minutes, the inviter probably moved on.
const SPAR_ROOM_TTL_SECS: i64 = 10 * 60;

/// Confirmed match results stay long enough to dedup duplicate client
/// reports during the natural retry window. After that, drop them — the
/// stats service has the durable copy.
const CONFIRMED_RESULT_TTL_SECS: i64 = 10 * 60;

/// Pending results (one client reported, partner hasn't yet) get a tighter
/// window. If the partner doesn't report within this, something went wrong
/// and we'd rather discard than commit the unverified half.
const PENDING_RESULT_TTL_SECS: i64 = 60;

/// Spectator frames older than this are no longer useful — the playing
/// peer either moved on or disconnected.
const SPECTATOR_FRAME_TTL_SECS: i64 = 90;

/// Lobby presence is a heartbeat; clients refresh every few seconds while
/// viewing the Online Hub.
const LOBBY_PRESENCE_TTL_SECS: i64 = 90;

/// General lobby chat is intentionally short-lived in the signaling process.
/// Durable chat history is not a goal for this server.
const LOBBY_CHAT_TTL_SECS: i64 = 30 * 60;

/// Direct challenges should expire quickly if the target doesn't accept.
const CHALLENGE_TTL_SECS: i64 = 2 * 60;

/// OAuth `state` nonces are short-lived; users complete the Discord round
/// trip in seconds, not minutes.
const OAUTH_STATE_TTL_SECS: i64 = 5 * 60;

/// How often the sweeper runs. Generous — there's no need to fire faster.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let counts = sweep_once(&state);
            if counts.total() > 0 {
                tracing::info!(
                    "[sweeper] reaped queue={} spar={} confirmed={} pending={} \
                     spectator={} lobby_presence={} lobby_chat={} challenges={} oauth={} \
                     lobbies={} (total {})",
                    counts.queue,
                    counts.spar,
                    counts.confirmed,
                    counts.pending,
                    counts.spectator,
                    counts.lobby_presence,
                    counts.lobby_chat,
                    counts.challenges,
                    counts.oauth,
                    counts.lobbies,
                    counts.total(),
                );
            }
        }
    });
}

#[derive(Default)]
struct SweepCounts {
    queue: usize,
    spar: usize,
    confirmed: usize,
    pending: usize,
    spectator: usize,
    lobby_presence: usize,
    lobby_chat: usize,
    challenges: usize,
    oauth: usize,
    lobbies: usize,
}

impl SweepCounts {
    fn total(&self) -> usize {
        self.queue
            + self.spar
            + self.confirmed
            + self.pending
            + self.spectator
            + self.lobby_presence
            + self.lobby_chat
            + self.challenges
            + self.oauth
            + self.lobbies
    }
}

fn sweep_once(state: &AppState) -> SweepCounts {
    let now = Utc::now();
    let mut out = SweepCounts::default();

    // queue: cancelled or stale-by-age. We collect keys first, then remove,
    // because dashmap's retain-while-iter would deadlock on shard locks held
    // by concurrent /match/lfg or /match/status callers.
    let stale: Vec<String> = state
        .queue
        .iter()
        .filter_map(|e| {
            let age = (now - e.queued_at).num_seconds();
            if age > QUEUE_TTL_SECS || (e.cancelled && age > 30) {
                Some(e.key().clone())
            } else {
                None
            }
        })
        .collect();
    for key in stale {
        if let Some((_, entry)) = state.queue.remove(&key) {
            // Detect "match never happened" cases that warrant an incident.
            //
            // - matched_no_result: queue entry was matched (had MatchInfo),
            //   wasn't cancelled, but no committed result for the room. Means
            //   the players paired, then something killed the session before
            //   /match/result was posted. Either side reporting a result
            //   would have transferred the room into confirmed_results.
            //
            // - lfg_no_pair: entry reached its TTL still in Queued state.
            //   Means matchmaking couldn't find a partner — different ROM
            //   hash, different app version, or just nobody else around.
            //   Less interesting on a quiet day but worth flagging.
            if !entry.cancelled {
                if let Some(info) = &entry.match_info {
                    let confirmed = state.confirmed_results.contains_key(&info.room_id)
                        || state
                            .confirmed_results
                            .iter()
                            .any(|r| r.key().starts_with(&format!("{}#", info.room_id)));
                    if !confirmed {
                        record_server_incident(
                            state,
                            ServerIncident {
                                kind: "matched_no_result".into(),
                                summary: format!(
                                    "Match {} between {} and {} reaped after {}s with no result",
                                    info.room_id,
                                    entry.username,
                                    info.username,
                                    (now - entry.queued_at).num_seconds(),
                                ),
                                room_id: info.room_id.clone(),
                                session_ids: vec![entry.session_id.clone()],
                                usernames: vec![entry.username.clone(), info.username.clone()],
                                details: serde_json::json!({
                                    "role": format!("{:?}", info.role),
                                    "peer_endpoint": info.peer_endpoint,
                                    "punch_at_ms": info.punch_at_ms,
                                    "had_turn_creds": info.turn.is_some(),
                                    "rom_hash": entry.rom_hash,
                                    "app_version": entry.app_version,
                                    "queued_at": entry.queued_at.to_rfc3339(),
                                    "reaped_at": now.to_rfc3339(),
                                }),
                            },
                        );
                    }
                } else {
                    record_server_incident(
                        state,
                        ServerIncident {
                            kind: "lfg_no_pair".into(),
                            summary: format!(
                                "{} sat in queue {}s without finding a partner (rom={}, app={})",
                                entry.username,
                                (now - entry.queued_at).num_seconds(),
                                entry.rom_hash,
                                entry.app_version,
                            ),
                            room_id: String::new(),
                            session_ids: vec![entry.session_id.clone()],
                            usernames: vec![entry.username.clone()],
                            details: serde_json::json!({
                                "rom_hash": entry.rom_hash,
                                "app_version": entry.app_version,
                                "stun_endpoint": entry.stun_endpoint,
                                "queued_at": entry.queued_at.to_rfc3339(),
                                "reaped_at": now.to_rfc3339(),
                            }),
                        },
                    );
                }
            }

            // If this player still has us as their session pointer, drop it.
            // Avoids a future LFG seeing a "stuck" session_id.
            if let Some(sid) = state
                .player_sessions
                .get(&entry.discord_id)
                .map(|s| s.clone())
            {
                if sid == key {
                    state.player_sessions.remove(&entry.discord_id);
                }
            }
            out.queue += 1;
        }
    }

    let stale: Vec<String> = state
        .spar_rooms
        .iter()
        .filter_map(|e| {
            if (now - e.created_at).num_seconds() > SPAR_ROOM_TTL_SECS {
                Some(e.key().clone())
            } else {
                None
            }
        })
        .collect();
    for key in stale {
        if state.spar_rooms.remove(&key).is_some() {
            out.spar += 1;
        }
    }

    let stale: Vec<String> = state
        .confirmed_results
        .iter()
        .filter_map(|e| {
            if (now - e.committed_at).num_seconds() > CONFIRMED_RESULT_TTL_SECS {
                Some(e.key().clone())
            } else {
                None
            }
        })
        .collect();
    for key in stale {
        if state.confirmed_results.remove(&key).is_some() {
            out.confirmed += 1;
        }
    }

    let stale: Vec<String> = state
        .pending_results
        .iter()
        .filter_map(|e| {
            if (now - e.reported_at).num_seconds() > PENDING_RESULT_TTL_SECS {
                Some(e.key().clone())
            } else {
                None
            }
        })
        .collect();
    for key in stale {
        if state.pending_results.remove(&key).is_some() {
            out.pending += 1;
        }
    }

    let stale: Vec<String> = state
        .spectator_frames
        .iter()
        .filter_map(|e| {
            if (now - e.updated_at).num_seconds() > SPECTATOR_FRAME_TTL_SECS {
                Some(e.key().clone())
            } else {
                None
            }
        })
        .collect();
    for key in stale {
        if state.spectator_frames.remove(&key).is_some() {
            out.spectator += 1;
        }
    }

    let stale: Vec<String> = state
        .lobby_presence
        .iter()
        .filter_map(|e| {
            if (now - e.last_seen).num_seconds() > LOBBY_PRESENCE_TTL_SECS {
                Some(e.key().clone())
            } else {
                None
            }
        })
        .collect();
    for key in stale {
        if state.lobby_presence.remove(&key).is_some() {
            out.lobby_presence += 1;
        }
    }

    let stale: Vec<String> = state
        .lobby_chat
        .iter()
        .filter_map(|e| {
            if (now - e.created_at).num_seconds() > LOBBY_CHAT_TTL_SECS {
                Some(e.key().clone())
            } else {
                None
            }
        })
        .collect();
    for key in stale {
        if state.lobby_chat.remove(&key).is_some() {
            out.lobby_chat += 1;
        }
    }

    let stale: Vec<String> = state
        .challenges
        .iter()
        .filter_map(|e| {
            if (now - e.created_at).num_seconds() > CHALLENGE_TTL_SECS {
                Some(e.key().clone())
            } else {
                None
            }
        })
        .collect();
    for key in stale {
        if let Some((_, challenge)) = state.challenges.remove(&key) {
            if let Some(mut entry) = state.queue.get_mut(&challenge.challenger_session_id) {
                entry.cancelled = true;
            }
            out.challenges += 1;
        }
    }

    let stale: Vec<String> = state
        .oauth_states
        .iter()
        .filter_map(|e| {
            if (now - e.issued_at).num_seconds() > OAUTH_STATE_TTL_SECS {
                Some(e.key().clone())
            } else {
                None
            }
        })
        .collect();
    for key in stale {
        if state.oauth_states.remove(&key).is_some() {
            out.oauth += 1;
        }
    }

    out.lobbies = crate::matchmaking::sweep_lobbies(state);

    out
}
