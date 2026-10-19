use crate::game;
use crate::state::AppState;

pub async fn notify_lfg(state: &AppState, username: &str, game: &str) {
    let Some(url) = &state.config.discord_webhook_url else {
        return;
    };
    let payload = serde_json::json!({
        "content": format!("🕹️ **{}** is looking for a match!{}", username, game::webhook_suffix(game)),
        "username": "Matchmaker"
    });
    if let Err(e) = state.http.post(url).json(&payload).send().await {
        tracing::warn!("Discord webhook failed: {}", e);
    }
}

pub async fn notify_matched(state: &AppState, u1: &str, u2: &str, game: &str) {
    let Some(url) = &state.config.discord_webhook_url else {
        return;
    };
    let payload = serde_json::json!({
        "content": format!("⚡ **{}** vs **{}** — match found!{}", u1, u2, game::webhook_suffix(game)),
        "username": "Matchmaker"
    });
    if let Err(e) = state.http.post(url).json(&payload).send().await {
        tracing::warn!("Discord webhook failed: {}", e);
    }
}
