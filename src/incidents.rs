//! Incident logging — captures the state of failed matches into a GCS
//! bucket so we can investigate "why didn't a match happen" without
//! relying on players to ship logs.
//!
//! Two surfaces:
//! 1. POST /match/incident — clients upload their own view of a failed
//!    match (their freeplay-net.log tail, their RAM scores, etc.).
//! 2. record_server_incident() — the server's own internal incident
//!    publisher. Called from the score-mismatch path, the sweeper's
//!    "stuck queue entry" detection, etc.
//!
//! Storage layout:
//!     gs://quarterframe-freeplay-incidents/YYYY/MM/DD/<incident_id>.json
//!
//! Day-partitioning keeps `gsutil ls` reasonable as the bucket grows.
//! The 90-day lifecycle policy on the bucket auto-deletes older files,
//! so we don't pay storage indefinitely.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::state::AppState;

/// Bucket name. Hardcoded rather than env-var because there's only one
/// for the lifetime of the project, and a typo in env config that
/// silently routes incidents to /dev/null is worse than a recompile.
const BUCKET: &str = "quarterframe-freeplay-incidents";

/// Truncate uploaded log payloads. Each entry has a hard cap to keep
/// the bucket from being weaponizable as cheap storage. 256 KB matches
/// the client-side cap.
const MAX_LOG_BYTES: usize = 256 * 1024;

/// What the client sends on /match/incident. All fields optional so
/// older clients (or partial captures) still produce useful records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentRequest {
    /// Free-form category. Examples: "hole_punch_failed",
    /// "ggrs_disconnected", "score_mismatch_rejected", "panic".
    pub kind: String,
    /// Best-effort short error description. Single line, displayed first
    /// in the bucket browser.
    #[serde(default)]
    pub summary: String,
    /// session_id of the failed match, if known.
    #[serde(default)]
    pub session_id: String,
    /// room_id of the failed match, if known.
    #[serde(default)]
    pub room_id: String,
    /// Peer's STUN endpoint (already known to matchmaking; not new PII).
    #[serde(default)]
    pub peer_endpoint: String,
    /// "host" or "join" — the reporting client's role.
    #[serde(default)]
    pub role: String,
    /// Reporting client's app version (e.g. "0.4.5").
    #[serde(default)]
    pub app_version: String,
    /// Reporting client's build timestamp (from FREEPLAY_BUILD_DATE).
    #[serde(default)]
    pub build_date: String,
    /// Reporting client's git short hash.
    #[serde(default)]
    pub git_hash: String,
    /// ROM hash from the client's mk2.zip.
    #[serde(default)]
    pub rom_hash: String,
    /// Stable per-install id. Lets us attribute anonymous reports (a crash
    /// before the player ever logged in) to an install without a Discord id.
    #[serde(default)]
    pub guest_device_id: String,
    /// Final scores from RAM, if the match got that far.
    #[serde(default)]
    pub p1_score: Option<u16>,
    #[serde(default)]
    pub p2_score: Option<u16>,
    /// How many GGRS frames advanced before failure (0 = never started).
    #[serde(default)]
    pub frames_advanced: u32,
    /// Tail of freeplay-net.log. Capped at 256 KB by the client.
    #[serde(default)]
    pub net_log_tail: String,
    /// Tail of GGRS event log (Synchronizing, NetworkInterrupted, etc.).
    #[serde(default)]
    pub ggrs_event_tail: String,
}

/// Server-side incidents are simpler — we don't have client state, but
/// we do have everything matchmaking knows about the room. Building this
/// as a separate type rather than reusing IncidentRequest keeps the
/// "where did this incident come from" clear in the stored JSON.
#[derive(Debug, Clone, Serialize)]
pub struct ServerIncident {
    pub kind: String,
    pub summary: String,
    pub room_id: String,
    pub session_ids: Vec<String>,
    pub usernames: Vec<String>,
    pub details: serde_json::Value,
}

/// A complete incident record as written to GCS. Wraps the inbound
/// request with server-controlled metadata that clients can't forge.
#[derive(Debug, Serialize)]
pub struct StoredIncident {
    pub incident_id: String,
    pub origin: &'static str, // "client" or "server"
    pub recorded_at: DateTime<Utc>,
    /// Verified Discord ID (from the JWT, not the request body).
    pub reporter_discord_id: Option<String>,
    pub reporter_username: Option<String>,
    /// Per-install id for anonymous (unauthenticated) reports.
    pub reporter_guest_id: Option<String>,
    pub payload: serde_json::Value,
}

/// POST /match/incident handler. Auth is optional: a valid JWT attributes the
/// report to a Discord identity, but unauthenticated reports are accepted too
/// (attributed to the client's guest_device_id) so a crash before the player
/// ever logged in still reaches the bucket. We can't count on players being
/// signed in when something breaks.
///
/// Returns 200 even on storage failure — incident-logging that can fail
/// the player's session is worse than a silently-dropped incident. We
/// log the error server-side; the bucket's job is best-effort capture.
pub async fn submit_incident(
    state: axum::extract::State<AppState>,
    auth: Option<
        axum_extra::TypedHeader<
            axum_extra::headers::Authorization<axum_extra::headers::authorization::Bearer>,
        >,
    >,
    body: axum::Json<IncidentRequest>,
) -> Result<axum::Json<serde_json::Value>, crate::error::AppError> {
    // Verify the token only if one was supplied; an invalid token is treated
    // as anonymous rather than rejected, so a stale JWT can't swallow a crash.
    let claims = auth
        .and_then(|a| crate::auth::verify_token(a.token(), &state.config.jwt_secret).ok());

    let mut req = body.0;
    truncate_field(&mut req.net_log_tail, MAX_LOG_BYTES);
    truncate_field(&mut req.ggrs_event_tail, MAX_LOG_BYTES);

    let reporter_guest_id = if req.guest_device_id.trim().is_empty() {
        None
    } else {
        Some(req.guest_device_id.clone())
    };

    let payload = serde_json::to_value(&req).unwrap_or(serde_json::Value::Null);
    let stored = StoredIncident {
        incident_id: Uuid::new_v4().to_string(),
        origin: "client",
        recorded_at: Utc::now(),
        reporter_discord_id: claims.as_ref().map(|c| c.sub.clone()),
        reporter_username: claims.as_ref().map(|c| c.username.clone()),
        reporter_guest_id,
        payload,
    };

    // Fire-and-forget the upload. Returning 200 immediately means the
    // client doesn't sit waiting on GCS round-trip during what is
    // already a failure cleanup path.
    let state = state.0.clone();
    tokio::spawn(async move {
        match upload_incident(&state, &stored).await {
            Ok(object_name) => {
                if let Err(e) = publish_github_issue(&state, &stored, &object_name).await {
                    tracing::warn!("[incident] github issue publish failed: {}", e);
                }
            }
            Err(e) => {
                tracing::error!(
                    "[incident] upload failed (kind={}, room={}): {}",
                    stored
                        .payload
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?"),
                    stored
                        .payload
                        .get("room_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?"),
                    e,
                );
            }
        }
    });

    Ok(axum::Json(json!({"ok": true})))
}

/// Server-internal incident publisher. Called from places where the
/// signaling server detects an abnormal condition without a client
/// reporting it (score mismatch, stuck queue entries, sweeper finds
/// orphaned room).
///
/// Fire-and-forget: returns immediately, uploads in a tokio::spawn.
pub fn record_server_incident(state: &AppState, incident: ServerIncident) {
    let stored = StoredIncident {
        incident_id: Uuid::new_v4().to_string(),
        origin: "server",
        recorded_at: Utc::now(),
        reporter_discord_id: None,
        reporter_username: None,
        reporter_guest_id: None,
        payload: serde_json::to_value(incident).unwrap_or(serde_json::Value::Null),
    };
    let state = state.clone();
    tokio::spawn(async move {
        match upload_incident(&state, &stored).await {
            Ok(object_name) => {
                if let Err(e) = publish_github_issue(&state, &stored, &object_name).await {
                    tracing::warn!("[incident] server github issue publish failed: {}", e);
                }
            }
            Err(e) => {
                tracing::error!("[incident] server upload failed: {}", e);
            }
        }
    });
}

fn truncate_field(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    // Keep the most recent content (i.e. truncate from the front). The
    // tail of a log is where the failure usually shows.
    let cut_from = s.len() - max_bytes;
    // Find the next char boundary so we don't slice mid-utf8.
    let mut start = cut_from;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    let kept = s[start..].to_string();
    *s = format!("[truncated {} bytes from front]\n{}", start, kept);
}

async fn upload_incident(state: &AppState, incident: &StoredIncident) -> anyhow::Result<String> {
    let token = fetch_metadata_token(&state.http).await?;

    let date = incident.recorded_at;
    let object_name = format!(
        "{:04}/{:02}/{:02}/{}.json",
        date.format("%Y").to_string().parse::<i32>().unwrap_or(0),
        date.format("%m").to_string().parse::<i32>().unwrap_or(0),
        date.format("%d").to_string().parse::<i32>().unwrap_or(0),
        incident.incident_id,
    );

    let body = serde_json::to_vec_pretty(incident)?;
    let url = format!(
        "https://storage.googleapis.com/upload/storage/v1/b/{}/o?uploadType=media&name={}",
        BUCKET,
        urlencoding::encode(&object_name),
    );

    let resp = state
        .http
        .post(&url)
        .bearer_auth(&token)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("GCS upload {}: {}", status, text);
    }
    tracing::info!(
        "[incident] uploaded {} (kind={}, origin={})",
        object_name,
        incident
            .payload
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        incident.origin,
    );
    Ok(object_name)
}

async fn publish_github_issue(
    state: &AppState,
    incident: &StoredIncident,
    object_name: &str,
) -> anyhow::Result<()> {
    let Some(repo) = state.config.github_issues_repo.as_deref() else {
        return Ok(());
    };
    let Some(token) = state.config.github_issues_token.as_deref() else {
        return Ok(());
    };

    let fingerprint = incident_fingerprint(incident);
    let title = incident_title(incident);
    let body = incident_issue_body(incident, object_name, &fingerprint);

    if let Some(issue) = find_open_issue(state, repo, token, &fingerprint).await? {
        let comment = format!(
            "Another matching incident arrived.\n\n- Incident: `{}`\n- Bucket object: `gs://{}/{}`\n- Recorded: `{}`",
            incident.incident_id,
            BUCKET,
            object_name,
            incident.recorded_at.to_rfc3339(),
        );
        let url = format!(
            "https://api.github.com/repos/{}/issues/{}/comments",
            repo, issue.number,
        );
        state
            .http
            .post(url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "freeplay-signaling-server")
            .json(&json!({ "body": comment }))
            .send()
            .await?
            .error_for_status()?;
        tracing::info!(
            "[incident] appended github issue #{} for fingerprint {}",
            issue.number,
            fingerprint,
        );
        return Ok(());
    }

    let url = format!("https://api.github.com/repos/{}/issues", repo);
    state
        .http
        .post(url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "freeplay-signaling-server")
        .json(&json!({
            "title": title,
            "body": body,
        }))
        .send()
        .await?
        .error_for_status()?;
    tracing::info!(
        "[incident] created github issue for fingerprint {}",
        fingerprint
    );
    Ok(())
}

#[derive(Deserialize)]
struct IssueSearch {
    items: Vec<IssueSearchItem>,
}

#[derive(Deserialize)]
struct IssueSearchItem {
    number: u64,
}

async fn find_open_issue(
    state: &AppState,
    repo: &str,
    token: &str,
    fingerprint: &str,
) -> anyhow::Result<Option<IssueSearchItem>> {
    let query = format!("repo:{} is:issue is:open {}", repo, fingerprint);
    let url = format!(
        "https://api.github.com/search/issues?q={}",
        urlencoding::encode(&query),
    );
    let resp = state
        .http
        .get(url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "freeplay-signaling-server")
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("github issue search {}: {}", status, text);
    }
    let found: IssueSearch = resp.json().await?;
    Ok(found.items.into_iter().next())
}

fn incident_title(incident: &StoredIncident) -> String {
    let payload = &incident.payload;
    let kind = payload
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let version = payload
        .get("app_version")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let summary = payload
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if version.is_empty() {
        format!("[incident] {}: {}", kind, one_line(summary, 100))
    } else {
        format!(
            "[incident] {} v{}: {}",
            kind,
            version,
            one_line(summary, 90)
        )
    }
}

fn incident_issue_body(incident: &StoredIncident, object_name: &str, fingerprint: &str) -> String {
    let payload = &incident.payload;
    let kind = payload
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let summary = payload
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let app_version = payload
        .get("app_version")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let git_hash = payload
        .get("git_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let room_id = payload
        .get("room_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let frames = payload
        .get("frames_advanced")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reporter = incident
        .reporter_username
        .as_deref()
        .or_else(|| {
            incident
                .reporter_guest_id
                .as_deref()
                .map(|_| "guest")
                .filter(|_| incident.origin == "client")
        })
        .unwrap_or("server");

    format!(
        "Automated incident from freeplay signaling.\n\n\
        - Fingerprint: `{}`\n\
        - Kind: `{}`\n\
        - Summary: `{}`\n\
        - Origin: `{}`\n\
        - Reporter: `{}`\n\
        - Version: `{}` `{}`\n\
        - Room: `{}`\n\
        - Session: `{}`\n\
        - Role: `{}`\n\
        - Frames advanced: `{}`\n\
        - Recorded: `{}`\n\
        - Bucket object: `gs://{}/{}`\n\n\
        Full client logs are intentionally kept in the private GCS incident object, not pasted into this public issue.",
        fingerprint,
        kind,
        one_line(summary, 500),
        incident.origin,
        reporter,
        app_version,
        git_hash,
        room_id,
        session_id,
        role,
        frames,
        incident.recorded_at.to_rfc3339(),
        BUCKET,
        object_name,
    )
}

fn incident_fingerprint(incident: &StoredIncident) -> String {
    let payload = &incident.payload;
    let kind = payload
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let version = payload
        .get("app_version")
        .and_then(|v| v.as_str())
        .unwrap_or("server");
    let summary = payload
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    format!(
        "freeplay-incident:{}:{}:{}",
        sanitize_label(kind),
        sanitize_label(version),
        sanitize_label(&one_line(summary, 48)),
    )
}

fn sanitize_label(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "unknown".into()
    } else {
        out
    }
}

fn one_line(s: &str, max_chars: usize) -> String {
    let mut out = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > max_chars {
        out = out
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>();
        out.push('…');
    }
    out
}

/// Fetch an OAuth2 access token from Cloud Run's metadata server.
/// On Cloud Run this is always available at metadata.google.internal.
/// Returns the bearer token to attach to the GCS request.
async fn fetch_metadata_token(http: &reqwest::Client) -> anyhow::Result<String> {
    #[derive(Deserialize)]
    struct TokenResp {
        access_token: String,
    }
    let resp: TokenResp = http
        .get("http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token")
        .header("Metadata-Flavor", "Google")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.access_token)
}
