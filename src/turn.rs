//! TURN REST API credential minting (RFC draft-uberti-rtcweb-turn-rest-00).
//!
//! The signaling server issues short-lived TURN credentials to clients at
//! match time. The TURN server (coturn with `use-auth-secret`) validates
//! them without any back-channel — it just recomputes the HMAC.
//!
//! Username format: `<unix-timestamp-seconds>:<session-id>`
//! Password:        `base64(hmac_sha1(shared_secret, username))`
//!
//! The timestamp is when the credential EXPIRES (not when it was issued).
//! coturn rejects usernames whose timestamp is in the past.

use hmac::{Hmac, Mac};
use sha1::Sha1;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::TurnCredentials;

type HmacSha1 = Hmac<Sha1>;

/// Mint a TURN credential that expires `ttl_secs` from now.
/// Typical TTL for a match: 1 hour (3600s).
pub fn mint_credentials(
    shared_secret: &str,
    turn_server_ip: &str,
    session_id: &str,
    ttl_secs: u64,
) -> TurnCredentials {
    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + ttl_secs;

    // Username is <expiry>:<session-id> — coturn parses the timestamp before the colon
    let username = format!("{}:{}", expiry, session_id);

    // HMAC-SHA1 over the username, key = shared secret, then base64 encode
    let mut mac = HmacSha1::new_from_slice(shared_secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(username.as_bytes());
    let password = B64.encode(mac.finalize().into_bytes());

    TurnCredentials {
        uri: format!("turn:{}:3478?transport=udp", turn_server_ip),
        username,
        password,
        ttl_secs,
    }
}
