# Freeplay Signaling Server

Axum-based HTTP service running on GCP Cloud Run that handles Discord OAuth, matchmaking queue, TURN credential minting, and match result forwarding for a netplay client.

> **Note:** This is one piece of a small Discord-authenticated netplay stack. It runs as a hobby project; the deploy scripts are tuned for a single GCP project but should be readable as a reference for similar setups.

## License

MIT — see [LICENSE](LICENSE).

## Architecture

```
┌──────────┐     ┌──────────────────┐     ┌──────────────┐
│  Client  │────▶│ Signaling Server │────▶│ Stats Service│
│ (freeplay)│    │  (Cloud Run)     │     │  (Cloud Run) │
└──────────┘     └──────────────────┘     └──────────────┘
                        │                        │
                        ▼                        ▼
                  ┌──────────┐            ┌───────────┐
                  │ TURN VM  │            │  SQLite   │
                  │ (coturn) │            │(GCS mount)│
                  └──────────┘            └───────────┘
```

- **Signaling Server** — this repo. Match broker: OAuth, queue, hole-punch coordination, TURN credential minting, result forwarding.
- **Stats Service** — companion repo (not included). Glicko-2 ratings, leaderboards, match history, Discord match announcements. SQLite persisted on Cloud Storage.
- **Client** — companion repo (not included). Emulator with GGRS rollback netcode, Discord OAuth, TURN relay client.

## Tech Stack

| Layer | Technology |
|---|---|
| Language | Rust (stable, edition 2021) |
| Web framework | Axum 0.7 |
| Async runtime | Tokio (full features) |
| Concurrency | DashMap (sharded concurrent hashmap) |
| HTTP client | Reqwests 0.12 |
| Auth | Discord OAuth2 → HS256 JWT (jsonwebtoken) |
| Serialization | Serde + serde_json, chrono, uuid, base64 |
| TURN auth | HMAC-SHA1 (coturn REST API) |
| Observability | tracing + tracing-subscriber |
| Deployment | GCP Cloud Run (Docker, Cloud Build) |
| Secrets | GCP Secret Manager |

## Endpoints

| Method | Path | Auth | Purpose |
|---|---|---|---|
| `GET` | `/health` | None | Cloud Run health probe |
| `GET` | `/auth/discord` | None | Redirect to Discord OAuth consent |
| `GET` | `/auth/discord/callback` | None | Discord redirects here with `?code=` → mints JWT |
| `GET` | `/auth/me` | Bearer JWT | Returns user info from JWT |
| `POST` | `/match/lfg` | Bearer JWT | Join queue or pair with matching opponent |
| `GET` | `/match/status/:id` | Bearer JWT | Poll for match status |
| `POST` | `/match/cancel` | Bearer JWT | Cancel own queue entry |
| `POST` | `/match/turn-ready/:id` | Bearer JWT | Report TURN-relayed address |
| `GET` | `/match/peer-relay/:id` | Bearer JWT | Poll peer's TURN-relayed address |
| `POST` | `/match/result` | Bearer JWT | Client reports match outcome → forwards to stats service |
| `POST` | `/signal/*` | Bearer JWT | Stubs for future ICE/WebRTC signalling |

## Configuration

All values come from environment variables (injected by Secret Manager on Cloud Run, or `.env` for local dev).

| Variable | Required | Purpose |
|---|---|---|
| `DISCORD_CLIENT_ID` | Yes | Discord app client ID |
| `DISCORD_CLIENT_SECRET` | Yes | Discord app secret |
| `DISCORD_REDIRECT_URI` | No | OAuth callback URL (defaults to localhost) |
| `DISCORD_WEBHOOK_URL` | No | Discord webhook for match notifications |
| `JWT_SECRET` | Yes | HS256 signing key (48 bytes base64) |
| `TURN_SERVER_IP` | No | coturn VM public IP |
| `TURN_SHARED_SECRET` | No | Shared secret for coturn REST API auth |
| `STATS_SERVICE_URL` | No | URL of stats service `/results` endpoint |
| `STATS_API_KEY` | No | Shared key for service-to-service auth |
| `PORT` | No | Listen port (Cloud Run sets 8080) |
| `RUST_LOG` | No | Tracing filter (e.g. `signaling_server=debug`) |

## Module Layout

```
src/
├── main.rs          Router setup, CORS, tracing, Cloud Run port binding
├── auth.rs          Discord OAuth flow + JWT mint/verify
├── config.rs        Config from env vars
├── discord.rs       Discord webhook posting (fire-and-forget)
├── error.rs         AppError enum → axum IntoResponse
├── matchmaking.rs   LFG, status, cancel, TURN address exchange, result forwarding
├── models.rs        Request/response types + internal QueueEntry
├── state.rs         AppState with DashMaps (queue, player_sessions, match_results)
└── turn.rs          TURN credential minting (HMAC-SHA1 for coturn REST API)
```

## Local Dev

```bash
# Copy .env.example and fill in values
cp .env.example .env

# Run
cargo run
# Server listens on http://localhost:8080
```

For local dev with the client, set the Discord redirect URI to `http://localhost:8080/auth/discord/callback` so OAuth redirects back to localhost.

## Deployment

Set `PROJECT_ID`, `REGION`, `SERVICE_NAME` (and the Discord secrets) in `.env`, then:

```bash
# One command — Cloud Build + Cloud Run
bash deploy.sh
```

Deploy script handles:
1. GCP auth + billing preflight
2. Enables required APIs (Run, Secret Manager, Cloud Build, Container Registry)
3. Writes secrets to Secret Manager (preserves existing JWT_SECRET)
4. Grants compute service account access to secrets
5. Builds Docker image via Cloud Build
6. Deploys to Cloud Run (max 2 instances, 256Mi)
7. Updates `DISCORD_REDIRECT_URI` based on deployed service URL

### Deployment order for full stack

1. Deploy stats service first — get its `/results` URL
2. Add `STATS_SERVICE_URL` + `STATS_API_KEY` to signaling server `.env`
3. `bash deploy.sh` in this repo

## Matchmaking Algorithm

```
Queue entry: { discord_id, stun_endpoint, app_version, rom_hash }

On LFG:
  1. Remove any existing session for this player
  2. Snapshot compatible opponents from queue (same app_version AND rom_hash)
  3. Atomically claim first available opponent via get_mut + re-check
  4. If paired: generate room_id + punch_at_ms + TURN creds, return Matched
  5. If no opponent: insert self into queue, return Queued
```

Only players with matching `app_version` **and** `rom_hash` can pair — mismatched ROM hashes would desync GGRS immediately.

Both clients then hole-punch at synchronized `punch_at_ms` (~2.5s in future). If direct P2P fails and TURN is configured, clients fall back to relay.

## Match Result Flow

```
Client A ──POST /match/result──▶ Signaling Server
Client B ──POST /match/result──▶      │
 (both report same                    │ dedup by room_id
  GGRS-synced scores)                 │ map Host=P1 / Join=P2
                                      ▼
                               Stats Service
                              (Glicko-2 update
                               + Discord post)
```

P1 in game RAM = Host (GGRS handle 0), P2 = Join (GGRS handle 1). Both clients see the same synced state, so the signaling server deduplicates by `room_id`.

## Design Decisions

- **In-memory state** (`DashMap`) — fine for 2 Cloud Run instances with session affinity. Match pairs are short-lived.
- **Fire-and-forget** — Discord webhooks and stats forwarding use `tokio::spawn`, never block handler response.
- **TOCTOU-safe matching** — candidate keys are snapshotted, then atomically claimed via `get_mut` with re-check under the same shard lock.
- **JWT preserved across redeploys** — `deploy.sh` only generates fresh `JWT_SECRET` if none exists. Rotating it invalidates every cached client token.

## Future Considerations

### Queue TTL / stale entry cleanup
Crashed players orphan queue entries forever. Add a background task to evict entries older than N minutes.

### Shared state for multi-instance
Currently two Cloud Run instances have independent queues. If horizontal scaling is needed, add Redis or Firestore for shared queue state.

### Client username in result forwarding
The stats service currently sees only Discord IDs in match results. Add a `username` field to the forward payload so leaderboards display names without the stats service needing to fetch them separately.

### ICE/WebRTC signalling
The `/signal/*` endpoints are stubs. Implementing WebRTC would eliminate the need for the TURN VM and improve connectivity through NAT.

### TURN allocation refresh
TURN credentials have a 1-hour TTL. Long matches need credential refresh from the client side.

### Regional deployments
Add a `region` field to LFG requests and pair within the same region for lower latency.

### Rate limiting
No rate limiting on auth or matchmaking endpoints. Add tower-governor or similar for production hardening.

## Watching Logs

```bash
gcloud beta run services logs tail $SERVICE_NAME \
  --region=$REGION \
  --project=$PROJECT_ID
```

Key log lines:
- `Queued <username> (rom=...)` — player entered queue
- `Matched <a> vs <b> (rom=...)` — pair found
- `[turn-ready] <username> relayed_addr=...` — client opened TURN allocation
- `Match result: <winner> beat <loser> — scores X:Y` — result processed
