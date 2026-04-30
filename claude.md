# Freeplay Signaling Server

Axum-based HTTP service running on Cloud Run that handles Discord OAuth, matchmaking queue, and TURN credential minting + address exchange for the Freeplay client.

## Project facts

- **Crate name:** `signaling-server`
- **Toolchain:** stable Rust, edition 2021
- **Runtime:** tokio (full features), axum 0.7, jsonwebtoken
- **Hosting:** Cloud Run (us-central1) — service name and project come from `deploy.sh`
- **Image:** `gcr.io/$PROJECT_ID/$SERVICE_NAME`
- **TURN server:** GCE e2-micro VM running coturn with REST API auth. IP stored in `TURN_SERVER_IP` secret.

## Architecture

`main.rs` wires up the Axum router, attaches CORS + tracing, loads config from env, and binds to `0.0.0.0:$PORT` (Cloud Run injects PORT=8080).

State is in-memory (`DashMap`) — perfectly fine for one Cloud Run instance with `--max-instances=2` since matches are short-lived. If we ever scale beyond two instances, we'd need Redis or similar for shared state. The two instances **do not share state** — but session affinity via Cloud Run's container-id-aware load balancing usually keeps both peers on the same instance during their match.

## Module layout

```
src/
├── main.rs           Router setup, CORS, tracing, .with_state(), Cloud Run port binding
├── auth.rs           Discord OAuth flow (login redirect, callback, JWT minting + verification)
├── config.rs         Config struct loaded from env vars (Discord creds, JWT secret, TURN config)
├── discord.rs        Discord webhook posts (notify_lfg, notify_matched). Optional — silent if DISCORD_WEBHOOK_URL is empty.
├── error.rs          AppError enum with axum IntoResponse impl
├── matchmaking.rs    /match/lfg, /match/status/:sid, /match/cancel, /match/turn-ready/:sid, /match/peer-relay/:sid handlers
├── models.rs         Request/response types (LfgRequest, MatchInfo, TurnCredentials, etc.) + internal QueueEntry
├── state.rs          AppState with DashMap<sid, QueueEntry> queue and DashMap<discord_id, sid> player_sessions
└── turn.rs           mint_credentials() — generates time-limited TURN username + HMAC-SHA1 password for coturn REST API auth
```

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | Cloud Run health probe — returns "ok" |
| GET | `/auth/discord` | Redirects to Discord OAuth consent |
| GET | `/auth/discord/callback` | Discord redirects back here with `?code=...` — exchanges for token, fetches user, mints JWT, redirects to `http://localhost:19420#token=<JWT>` |
| GET | `/auth/me` | Returns user info from JWT (debug) |
| POST | `/match/lfg` | `{stun_endpoint, app_version, rom_hash}` — joins queue or pairs with existing entry having matching app_version+rom_hash |
| GET | `/match/status/:session_id` | Poll for match status — returns `{status, match_info?}` |
| POST | `/match/cancel` | Mark current player's session as cancelled |
| POST | `/match/turn-ready/:session_id` | `{relayed_addr}` — record this client's TURN-allocated address |
| GET | `/match/peer-relay/:session_id` | Returns peer's relayed_addr if reported, else null |
| POST `/signal/*` | (stubs — kept for future ICE work) |

## Authentication

- Auth header: `Authorization: Bearer <JWT>` on every match endpoint
- JWT uses HS256, signed with `JWT_SECRET` (random 48-byte base64). 24-hour expiry.
- Claims: `{sub: discord_id, username, exp}`

## Matchmaking algorithm

```rust
// In matchmaking::looking_for_game
let opponent_key = state.queue.iter().find(|e| {
    !e.cancelled
        && e.match_info.is_none()
        && e.discord_id != claims.sub
        && e.app_version == req.app_version
        && e.rom_hash == req.rom_hash
}).map(|e| e.key().clone());
```

Pairs only with someone matching BOTH `app_version` AND `rom_hash`. Mismatched ROM hashes never pair (they'd desync ggrs immediately on session start). Pairing is FIFO within compatible cohort.

When pairing succeeds: a `room_id` is generated, both entries get a `MatchInfo` written with their respective roles (host vs join), each other's `peer_endpoint` (STUN), a synchronized `punch_at_ms` (~2.5 seconds in the future), and optionally `turn` credentials (if TURN is configured).

## TURN credential minting

`turn::mint_credentials(secret, ip, room_id, ttl_secs)`:

1. Username = `"<unix_expiry>:<room_id>"` — used by coturn to enforce TTL
2. Password = `base64(HMAC-SHA1(secret, username))` — server checks this matches without storing per-user passwords
3. URI = `"turn:<ip>:3478?transport=udp"`

This is the **REST API for Access to TURN Services** spec (draft-uberti-behave-turn-rest-00). coturn validates by recomputing the HMAC and checking the timestamp hasn't expired.

## TURN address exchange

When a match needs TURN (direct hole punch failed):

1. Both clients open their own TURN allocation independently using shared credentials
2. Each client POSTs its XOR-RELAYED-ADDRESS to `/match/turn-ready/:sid`  
3. The server stores `relayed_addr` on the QueueEntry
4. Each client polls `/match/peer-relay/:sid` — server finds the OTHER QueueEntry sharing the same `room_id` and returns its `relayed_addr`
5. Once both addresses are exchanged, each client adds a TURN permission for the peer's relayed address and routes GGRS traffic there

## Configuration / env vars (all from GCP Secret Manager)

| Variable | Purpose |
|---|---|
| `DISCORD_CLIENT_ID` | Discord app client ID |
| `DISCORD_CLIENT_SECRET` | Discord app secret |
| `DISCORD_REDIRECT_URI` | `https://<service>.run.app/auth/discord/callback` |
| `DISCORD_WEBHOOK_URL` | Optional. Empty string = no webhook. |
| `JWT_SECRET` | 48 random bytes, base64 |
| `TURN_SHARED_SECRET` | hex(32 bytes) — shared with coturn's `static-auth-secret` |
| `TURN_SERVER_IP` | Public IP of coturn VM |
| `PORT` | Set by Cloud Run automatically (8080) |
| `RUST_LOG` | e.g. `signaling_server=debug,tower_http=debug` |

`config::turn_enabled()` returns true only when both `TURN_SHARED_SECRET` and `TURN_SERVER_IP` are set. If false, MatchInfo's `turn` field is None and clients fall through to "no TURN configured" error.

## Local dev .env

```
DISCORD_CLIENT_ID=...
DISCORD_CLIENT_SECRET=...
DISCORD_REDIRECT_URI=https://<your-cloud-run-service>.run.app/auth/discord/callback
DISCORD_WEBHOOK_URL=https://discord.com/api/webhooks/...
JWT_SECRET=...
RUST_LOG=signaling_server=debug,tower_http=debug
```

The `.env` file is **read by deploy.sh** (not by the running server). The server reads from process env vars, which Cloud Run populates from Secret Manager. **`.env` MUST be gitignored** — it contains real Discord secrets.

## Deployment

Run `wsl bash deploy.sh` from the project root.

`deploy.sh` does:
1. Loads `.env` via `set -a; source .env; set +a` — validates DISCORD_CLIENT_ID isn't a placeholder
2. Preflight checks — gcloud auth, project access, billing enabled
3. Enables APIs (run, secretmanager, cloudbuild, containerregistry)
4. Writes Discord secrets to Secret Manager (Webhook is optional, JWT_SECRET preserved across redeploys)
5. Grants `roles/secretmanager.secretAccessor` to the compute service account `<project_number>-compute@developer.gserviceaccount.com`
6. Builds image via Cloud Build (`gcloud builds submit --tag gcr.io/$PROJECT_ID/$SERVICE_NAME`) — server-side Docker, ~3-5min first time, ~30s cached
7. Deploys to Cloud Run with all secrets bound via `--set-secrets` (TURN secrets included automatically if they exist)
8. Updates `DISCORD_REDIRECT_URI` based on the actual deployed service URL

`setup-turn.sh` (separate, run once) provisions the GCE coturn VM and writes TURN_SHARED_SECRET + TURN_SERVER_IP secrets. Subsequent `deploy.sh` runs detect those secrets and wire them into the Cloud Run service.

## Watching logs

```bash
gcloud beta run services logs tail $SERVICE_NAME --region=$REGION --project=$PROJECT_ID
```

Look for INFO lines:
- `Queued <username> (rom=...)` — entered queue
- `Matched <a> vs <b> (rom=...)` — paired
- `[turn-ready] <username> relayed_addr=...` — client reported its TURN allocation
- DEBUG lines from tower_http show every request

## Cargo.toml dependencies

```toml
axum = "0.7"
axum-extra = { version = "0.9", features = ["typed-header"] }
tokio = { version = "1", features = ["full"] }
tower-http = { version = "0.5", features = ["cors", "trace"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
jsonwebtoken = "9"
chrono = { version = "0.4", features = ["serde"] }
dashmap = "5"
reqwest = { version = "0.12", features = ["json"] }
uuid = { version = "1", features = ["v4", "serde"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
anyhow = "1"
thiserror = "1"
dotenvy = "0.15"
hmac = "0.12"
sha1 = "0.10"
base64 = "0.22"
urlencoding = "2"
```

## Critical invariants

- **JWT_SECRET preserved across redeploys.** `deploy.sh` only generates a fresh one if the secret doesn't exist yet. If JWT_SECRET rotates, every cached client token becomes invalid — the client gets 401 and has to re-OAuth.
- **Compute service account binding** uses `--condition=None` to avoid IAM condition errors that some setups produce.
- **Two players with mismatched rom_hash never pair**, even if both are in the queue forever. This is intentional — they would desync immediately. They sit in the queue until someone with the matching ROM joins.
- **`#[serde(default)]` on `LfgRequest::rom_hash`** preserves backward compat with old clients that don't send the field — they get `""` and only pair with each other.

## Known issues / TODOs

- State is in-memory only. Cloud Run instance restart = all queued players dropped. They'll re-queue automatically on next request.
- No queue timeout. If a player's client crashes between LFG and the matched response being polled, their entry sits forever. Could add a 5-minute TTL sweep.
- ICE/`/signal/*` endpoints are stubs returning `{ok: true}` and `{candidates: []}`. Reserved for future WebRTC-based path.
- TURN allocation refresh isn't implemented client-side, so matches longer than 10 minutes will lose their TURN allocation.

## Style notes

- Use `tracing::info!` for state-changing events (matched, queued, turn-ready), `debug!` for routine.
- `AppError` (in `error.rs`) is the only error type returned by handlers — it implements `IntoResponse` for clean axum integration.
- Handlers take `State<AppState>` by value (cheap clone — DashMap is Arc'd internally).
- Response types are in `models.rs` with `#[derive(Serialize, Deserialize)]` — never inline anonymous JSON.

## What NOT to do

- Don't add per-request blocking I/O. Use tokio async or spawn_blocking.
- Don't put business state in `Config` — that's a static snapshot of env vars.
- Don't change wire formats without bumping app_version (current pairing assumes both peers' formats match exactly).
- Don't drop the `Authorization: Bearer` requirement on match endpoints — anonymous matchmaking would let any random throw arbitrary `peer_endpoint` strings at users.
- Don't return inner error details in responses for failures (auth, validation). The current `AppError` is correctly stripped.