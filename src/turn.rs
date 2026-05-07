//! Relay credential minting for freeplay-relay.
//!
//! Replaces the previous coturn / TURN-REST-API minting. Same field
//! shape (uri/username/password/ttl_secs) so old MatchInfo wire format
//! is unchanged — but the contents now describe how to authenticate
//! to our custom UDP relay rather than to coturn.
//!
//! We use a `relay://` URI scheme so v0.5.0+ clients route to the new
//! relay protocol; legacy `turn:` clients (v0.4.x and earlier) get the
//! same shape and would attempt their broken coturn path. They're
//! already broken in production, so no regression.
//!
//! ## Field encoding (shared with the relay)
//!
//! - `uri = "relay://<ip>:<port>"`
//! - `username = "<role>:<expiry_unix_secs>:<room_id>"` — the same string
//!   the relay HMACs in REGISTER. Client just splits on `:` and feeds
//!   each part into its REGISTER packet.
//! - `password = hex(HMAC-SHA256(shared_secret, username))` — relay
//!   recomputes and compares.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::TurnCredentials;

type HmacSha256 = Hmac<Sha256>;

/// Mint a relay credential expiring `ttl_secs` from now.
/// `role` is 0 (host) or 1 (join). `room_id` is the match's UUID.
pub fn mint_credentials(
    shared_secret: &str,
    relay_server_ip: &str,
    room_id: &str,
    role: u8,
    ttl_secs: u64,
) -> TurnCredentials {
    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + ttl_secs;

    // <role>:<expiry>:<room_id>  — exact format the relay HMACs.
    let username = format!("{}:{}:{}", role, expiry, room_id);

    // HMAC the same payload with sha256 → hex.
    let secret_bytes =
        hex_decode(shared_secret).unwrap_or_else(|| shared_secret.as_bytes().to_vec());
    let mut mac = HmacSha256::new_from_slice(&secret_bytes).expect("hmac accepts any key");
    mac.update(username.as_bytes());
    let tag = mac.finalize().into_bytes();
    let password = hex_encode(&tag);

    TurnCredentials {
        uri: format!("relay://{}:3478", relay_server_ip),
        username,
        password,
        ttl_secs,
    }
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}
