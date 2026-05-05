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

use std::time::Duration;
use chrono::Utc;

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
                     spectator={} oauth={} (total {})",
                    counts.queue,
                    counts.spar,
                    counts.confirmed,
                    counts.pending,
                    counts.spectator,
                    counts.oauth,
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
    oauth: usize,
}

impl SweepCounts {
    fn total(&self) -> usize {
        self.queue + self.spar + self.confirmed + self.pending + self.spectator + self.oauth
    }
}

fn sweep_once(state: &AppState) -> SweepCounts {
    let now = Utc::now();
    let mut out = SweepCounts::default();

    // queue: cancelled or stale-by-age. We collect keys first, then remove,
    // because dashmap's retain-while-iter would deadlock on shard locks held
    // by concurrent /match/lfg or /match/status callers.
    let stale: Vec<String> = state.queue
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
            // If this player still has us as their session pointer, drop it.
            // Avoids a future LFG seeing a "stuck" session_id.
            if let Some(sid) = state.player_sessions.get(&entry.discord_id).map(|s| s.clone()) {
                if sid == key {
                    state.player_sessions.remove(&entry.discord_id);
                }
            }
            out.queue += 1;
        }
    }

    let stale: Vec<String> = state.spar_rooms
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

    let stale: Vec<String> = state.confirmed_results
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

    let stale: Vec<String> = state.pending_results
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

    let stale: Vec<String> = state.spectator_frames
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

    let stale: Vec<String> = state.oauth_states
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

    out
}
