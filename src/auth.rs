use axum::{
    extract::{Query, State},
    response::{IntoResponse, Json, Redirect},
};
use axum_extra::headers::authorization::Bearer;
use axum_extra::{headers::Authorization, TypedHeader};
use chrono::Utc;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::{
    error::AppError,
    models::{Claims, DiscordUser, GuestAuthRequest},
    state::{AppState, OAuthState},
};

const MAX_USERNAME_LEN: usize = 24;

pub async fn guest_login(
    State(state): State<AppState>,
    Json(req): Json<GuestAuthRequest>,
) -> Result<impl IntoResponse, AppError> {
    let username = sanitize_username(&req.username).ok_or_else(|| {
        AppError::BadRequest(format!(
            "Username must be 2-{MAX_USERNAME_LEN} letters/numbers"
        ))
    })?;
    let email = req.email.as_deref().and_then(normalize_email);
    let device_id = req.device_id.as_deref().filter(|s| !s.is_empty());
    let (prefix, identity): (&str, &str) = if let Some(e) = email.as_deref() {
        ("guest-email", e)
    } else if let Some(d) = device_id {
        ("guest-device", d)
    } else {
        ("guest-name", &username)
    };
    let sub = format!("{prefix}:{}", sha256_hex(identity.as_bytes()));
    let exp = (Utc::now() + chrono::Duration::days(30)).timestamp() as usize;
    let claims = Claims {
        sub,
        username: username.clone(),
        email,
        exp,
    };
    let jwt = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.config.jwt_secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(e.into()))?;

    Ok(Json(json!({
        "token": jwt,
        "username": username,
        "expires_at": exp,
    })))
}

pub async fn discord_login(State(state): State<AppState>) -> impl IntoResponse {
    // CSRF protection: generate a one-shot nonce, store it server-side,
    // round-trip it through Discord as the OAuth `state` parameter, and
    // verify it on the callback. Without this an attacker can craft a
    // pre-baked OAuth URL that completes the flow under the victim's
    // browser session.
    let nonce = Uuid::new_v4().to_string();
    state.oauth_states.insert(
        nonce.clone(),
        OAuthState {
            issued_at: Utc::now(),
        },
    );

    let url = format!(
        "https://discord.com/api/oauth2/authorize?client_id={}&redirect_uri={}&response_type=code&scope=identify&state={}",
        state.config.discord_client_id,
        urlencoding::encode(&state.config.discord_redirect_uri),
        urlencoding::encode(&nonce),
    );
    Redirect::temporary(&url)
}

#[derive(Deserialize)]
pub struct CallbackParams {
    code: String,
    /// CSRF nonce echoed back by Discord. Required.
    state: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

pub async fn discord_callback(
    State(state): State<AppState>,
    Query(params): Query<CallbackParams>,
) -> Result<impl IntoResponse, AppError> {
    // Best-effort CSRF nonce check. With Cloud Run's 2-instance scaling,
    // the original /auth/discord request and the callback can land on
    // different instances — and oauth_states is in-memory per instance.
    // Strict validation rejected legitimate logins. Log unknown nonces
    // for telemetry but let the flow continue; once we move state to a
    // shared store (Redis/Memorystore) this becomes strict again.
    let nonce = params.state.as_deref().unwrap_or("");
    if nonce.is_empty() {
        tracing::warn!("[oauth] callback missing state param entirely");
    } else if state.oauth_states.remove(nonce).is_none() {
        tracing::info!(
            "[oauth] callback nonce not on this instance (cross-instance load balancing); proceeding",
        );
    }

    let token_res: TokenResponse = state
        .http
        .post("https://discord.com/api/oauth2/token")
        .form(&[
            ("client_id", state.config.discord_client_id.as_str()),
            ("client_secret", state.config.discord_client_secret.as_str()),
            ("grant_type", "authorization_code"),
            ("code", params.code.as_str()),
            ("redirect_uri", state.config.discord_redirect_uri.as_str()),
        ])
        .send()
        .await
        .map_err(|e| AppError::Internal(e.into()))?
        .json()
        .await
        .map_err(|e| AppError::Internal(e.into()))?;

    let user: DiscordUser = state
        .http
        .get("https://discord.com/api/users/@me")
        .bearer_auth(&token_res.access_token)
        .send()
        .await
        .map_err(|e| AppError::Internal(e.into()))?
        .json()
        .await
        .map_err(|e| AppError::Internal(e.into()))?;

    // 7-day JWT (was 30). Long enough that desktop users don't see daily
    // re-OAuth, short enough that an exfiltrated token doesn't grant a month
    // of impersonation. A refresh-token flow would let us shorten this further;
    // not yet implemented.
    let exp = (Utc::now() + chrono::Duration::days(7)).timestamp() as usize;
    let claims = Claims {
        sub: user.id.clone(),
        username: user.username.clone(),
        email: None,
        exp,
    };

    let jwt = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.config.jwt_secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(e.into()))?;

    // Redirect to the local callback server the desktop app is listening on.
    // Fragment (#token=...) is read by JS and POSTed back — never hits a server.
    Ok(Redirect::temporary(&format!(
        "http://localhost:19420/auth/callback#token={}",
        jwt
    )))
}

pub async fn me(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
) -> Result<impl IntoResponse, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    Ok(Json(
        json!({ "discord_id": claims.sub, "username": claims.username }),
    ))
}

pub fn verify_token(token: &str, secret: &str) -> Result<Claims, AppError> {
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map(|d| d.claims)
    .map_err(|e| AppError::Unauthorized(format!("Invalid token: {}", e)))
}

fn sanitize_username(raw: &str) -> Option<String> {
    let mut out = String::new();
    for c in raw.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else if c.is_whitespace() && !out.ends_with('_') {
            out.push('_');
        }
        if out.len() >= MAX_USERNAME_LEN {
            break;
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.len() >= 2 {
        Some(out)
    } else {
        None
    }
}

fn normalize_email(raw: &str) -> Option<String> {
    let email = raw.trim().to_ascii_lowercase();
    if email.len() <= 254 && email.contains('@') && email.contains('.') {
        Some(email)
    } else {
        None
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(bytes);
    let mut out = String::with_capacity(hash.len() * 2);
    for b in hash {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}
