//! Which game a queue entry, room or lobby belongs to.
//!
//! Same convention as freeplay-stats (`src/game.rs` there): the game rides
//! inside `rom_hash`. A client running anything but MK2 prefixes it
//! (`nbajam:<fnv>@<core>`, `umk3w:<sha>`); an unprefixed hash is MK2, which
//! is what every client before multi-game support sent.
//!
//! Pairing already requires an exact `rom_hash` match, so two games can never
//! meet. This module only names the game for listings, logs and webhooks.

/// The game every pre-multi-game client belongs to.
pub const DEFAULT_GAME: &str = "mk2";

/// `?game=` value that asks a listing for every game at once.
pub const ALL_GAMES: &str = "all";

/// A game key is a short lowercase identifier, so a malformed prefix can't
/// invent a game.
fn valid_key(key: &str) -> bool {
    (1..=16).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// `nbajam:1a2b3c4d@cf53523` -> `nbajam`; `1a2b3c4d@cf53523` -> `mk2`.
pub fn from_rom_hash(rom_hash: &str) -> String {
    match rom_hash.split_once(':') {
        Some((key, _)) if valid_key(key) => key.to_string(),
        _ => DEFAULT_GAME.to_string(),
    }
}

/// `?game=` on a listing. Absent or invalid means MK2 (what every client that
/// predates the parameter is showing); `all` means no filter (`None`).
pub fn filter_from_query(game: Option<&str>) -> Option<String> {
    match game.map(str::trim) {
        Some(ALL_GAMES) => None,
        Some(key) if valid_key(key) => Some(key.to_string()),
        _ => Some(DEFAULT_GAME.to_string()),
    }
}

/// Webhook suffix naming the game, e.g. " (UMK3 Wavenet)". Empty for MK2 so
/// existing posts read exactly as before.
pub fn webhook_suffix(game: &str) -> String {
    match game {
        DEFAULT_GAME => String::new(),
        "umk3w" => " (UMK3 Wavenet)".to_string(),
        "umk3" => " (UMK3)".to_string(),
        "nbajam" => " (NBA Jam)".to_string(),
        other => format!(" ({other})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unprefixed_hash_is_mk2() {
        assert_eq!(from_rom_hash("1a2b3c4d5e6f7a8b"), "mk2");
        assert_eq!(from_rom_hash(""), "mk2");
    }

    #[test]
    fn prefixed_hash_names_its_game() {
        assert_eq!(from_rom_hash("umk3w:f8a424f63931510f"), "umk3w");
        assert_eq!(from_rom_hash("nbajam:1a2b@cf53523#9f8e"), "nbajam");
    }

    #[test]
    fn malformed_prefix_falls_back_to_mk2() {
        assert_eq!(from_rom_hash("UMK3W:abcd"), "mk2");
        assert_eq!(from_rom_hash("a-very-long-game-name-x:abcd"), "mk2");
    }

    #[test]
    fn listing_filter_defaults_to_mk2() {
        assert_eq!(filter_from_query(None).as_deref(), Some("mk2"));
        assert_eq!(filter_from_query(Some("umk3w")).as_deref(), Some("umk3w"));
        assert_eq!(filter_from_query(Some("all")), None);
        assert_eq!(filter_from_query(Some("Bad Key")).as_deref(), Some("mk2"));
    }
}
