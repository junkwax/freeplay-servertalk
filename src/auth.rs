use axum::{
    extract::{Query, State},
    response::{IntoResponse, Json, Redirect},
};
use axum_extra::headers::authorization::Bearer;
use axum_extra::{headers::Authorization, TypedHeader};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::Deserialize;
use serde_json::json;
use chrono::Utc;

use crate::{error::AppError, models::{Claims, DiscordUser}, state::AppState};

pub async fn discord_login(State(state): State<AppState>) -> impl IntoResponse {
    let url = format!(
        "https://discord.com/api/oauth2/authorize?client_id={}&redirect_uri={}&response_type=code&scope=identify",
        state.config.discord_client_id,
        urlencoding::encode(&state.config.discord_redirect_uri),
    );
    Redirect::temporary(&url)
}

#[derive(Deserialize)]
pub struct CallbackParams { code: String }

#[derive(Deserialize)]
struct TokenResponse { access_token: String }

pub async fn discord_callback(
    State(state): State<AppState>,
    Query(params): Query<CallbackParams>,
) -> Result<impl IntoResponse, AppError> {
    let token_res: TokenResponse = state.http
        .post("https://discord.com/api/oauth2/token")
        .form(&[
            ("client_id",     state.config.discord_client_id.as_str()),
            ("client_secret", state.config.discord_client_secret.as_str()),
            ("grant_type",    "authorization_code"),
            ("code",          params.code.as_str()),
            ("redirect_uri",  state.config.discord_redirect_uri.as_str()),
        ])
        .send().await.map_err(|e| AppError::Internal(e.into()))?
        .json().await.map_err(|e| AppError::Internal(e.into()))?;

    let user: DiscordUser = state.http
        .get("https://discord.com/api/users/@me")
        .bearer_auth(&token_res.access_token)
        .send().await.map_err(|e| AppError::Internal(e.into()))?
        .json().await.map_err(|e| AppError::Internal(e.into()))?;

    let exp = (Utc::now() + chrono::Duration::days(30)).timestamp() as usize;
    let claims = Claims { sub: user.id.clone(), username: user.username.clone(), exp };

    let jwt = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.config.jwt_secret.as_bytes()),
    ).map_err(|e| AppError::Internal(e.into()))?;

    // Redirect to the local callback server the desktop app is listening on.
    // Fragment (#token=...) is read by JS and POSTed back — never hits a server.
    Ok(Redirect::temporary(&format!("http://localhost:19420/auth/callback#token={}", jwt)))
}

pub async fn me(
    State(state): State<AppState>,
    TypedHeader(auth): TypedHeader<Authorization<Bearer>>,
) -> Result<impl IntoResponse, AppError> {
    let claims = verify_token(auth.token(), &state.config.jwt_secret)?;
    Ok(Json(json!({ "discord_id": claims.sub, "username": claims.username })))
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
