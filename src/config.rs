use anyhow::Context;

#[derive(Clone, Debug)]
pub struct Config {
    pub discord_client_id: String,
    pub discord_client_secret: String,
    pub discord_webhook_url: Option<String>,
    pub discord_redirect_uri: String,
    pub jwt_secret: String,

    // TURN relay fallback — optional, only enabled if both are set
    pub turn_server_ip: Option<String>,
    pub turn_shared_secret: Option<String>,
    pub turn_realm: String,

    // Stats service — forwards match results for Glicko-2 ranking / leaderboards
    pub stats_service_url: Option<String>,
    pub stats_api_key: Option<String>,

    // Optional GitHub issue publisher for incident reports. Keep the token
    // server-side; public clients only upload incidents to this service.
    pub github_issues_repo: Option<String>,
    pub github_issues_token: Option<String>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            discord_client_id: std::env::var("DISCORD_CLIENT_ID")
                .context("DISCORD_CLIENT_ID not set")?,
            discord_client_secret: std::env::var("DISCORD_CLIENT_SECRET")
                .context("DISCORD_CLIENT_SECRET not set")?,
            discord_webhook_url: std::env::var("DISCORD_WEBHOOK_URL").ok(),
            discord_redirect_uri: std::env::var("DISCORD_REDIRECT_URI")
                .unwrap_or_else(|_| "http://localhost:8080/auth/discord/callback".to_string()),
            jwt_secret: std::env::var("JWT_SECRET").context("JWT_SECRET not set")?,
            turn_server_ip: std::env::var("TURN_SERVER_IP").ok(),
            turn_shared_secret: std::env::var("TURN_SHARED_SECRET").ok(),
            turn_realm: std::env::var("TURN_REALM").unwrap_or_else(|_| "example.com".to_string()),
            stats_service_url: std::env::var("STATS_SERVICE_URL").ok(),
            stats_api_key: std::env::var("STATS_API_KEY").ok(),
            github_issues_repo: std::env::var("GITHUB_ISSUES_REPO").ok(),
            github_issues_token: std::env::var("GITHUB_ISSUES_TOKEN").ok(),
        })
    }

    /// Whether the TURN relay fallback is configured and ready to issue credentials
    pub fn turn_enabled(&self) -> bool {
        self.turn_server_ip.is_some() && self.turn_shared_secret.is_some()
    }
}
