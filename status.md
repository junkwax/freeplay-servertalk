# Freeplay Signaling Server — Status

Last updated: 2026-04-25

## Currently working

- ✅ Service deployed to Cloud Run
- ✅ Discord OAuth full flow (login redirect → callback → JWT mint → client redirect)
- ✅ JWT-based auth on `/match/*` endpoints (24h tokens)
- ✅ `/match/lfg` queue + matchmaking with `(app_version, rom_hash)` filter
- ✅ `/match/status/<sid>` polling
- ✅ `/match/cancel` clean cancellation
- ✅ TURN credentials minted on match (HMAC-SHA1 REST API for coturn)
- ✅ `deploy.sh` reads from `.env` properly, validates against placeholders
- ✅ JWT_SECRET preserved across redeploys (cached client tokens stay valid)
- ✅ Optional Discord webhook posts on LFG / match
- ✅ Fresh build deploys via Cloud Build (`gcloud builds submit`) — no local Docker auth headache

## Just shipped, NOT YET TESTED

- ⚠️ **`/match/turn-ready/:session_id`** (POST) — accepts `{relayed_addr}` for the calling client, stores on QueueEntry
- ⚠️ **`/match/peer-relay/:session_id`** (GET) — returns peer's relayed_addr if set, else null
- ⚠️ **`QueueEntry::relayed_addr: Option<String>`** field
- ⚠️ **rom_hash filter** in pairing logic — players with mismatched ROMs never pair (replaces the older app_version-only filter)

## Verifying after deploy

```powershell
# Confirm secrets are actually populated (not placeholders)
gcloud secrets versions access latest --secret=DISCORD_CLIENT_ID --project=$PROJECT_ID
gcloud secrets versions access latest --secret=TURN_SHARED_SECRET --project=$PROJECT_ID
gcloud secrets versions access latest --secret=TURN_SERVER_IP --project=$PROJECT_ID

# Check active revision serving traffic
gcloud run revisions list --service=$SERVICE_NAME --region=$REGION --project=$PROJECT_ID --limit=3

# Tail logs during a match attempt
gcloud beta run services logs tail $SERVICE_NAME --region=$REGION --project=$PROJECT_ID

# Test health endpoint
curl https://<your-service>.run.app/health
```

When two clients click Find Match in the new flow, expected log lines (in order):

```
INFO Queued <user_a> (rom=<hash>)
INFO Matched <user_a> vs <user_b> (rom=<hash>)
INFO [turn-ready] <user_a> relayed_addr=<ip>:49212
INFO [turn-ready] <user_b> relayed_addr=<ip>:49253
```

## Known gaps / TODOs

- **State is in-memory.** Cloud Run instance restart drops all queued players (they re-queue automatically on next request, so user-visible impact is minimal).
- **No queue TTL.** If a client crashes mid-match-setup, the entry persists. Adding a 5-minute background sweep would clean these up.
- **Max 2 instances** (`--max-instances=2` in deploy.sh) intentionally — we don't have shared state. If we ever need to scale beyond 2, need Redis for queue.
- **`/signal/*` endpoints are stubs** returning `{ok: true}` and `{candidates: []}`. Reserved for future ICE-based path.
- **No rate limiting** on any endpoint. A malicious client could spam `/match/lfg` to exhaust the queue. Tower has rate-limiting middleware available if needed.
- **No metrics export.** Cloud Run gives basic latency/error metrics in console but no per-match-outcome stats. Could add Prometheus exporter or push to Cloud Logging structured fields.
- **Webhook errors silent.** `discord::notify_*` is fire-and-forget — if Discord 5xxs, we never know.

## Files modified in this session

| File | Status |
|---|---|
| `src/models.rs` | Added `rom_hash` field to LfgRequest + QueueEntry; added `TurnReadyRequest` and `PeerRelayResponse` types; `relayed_addr: Option<String>` on QueueEntry |
| `src/matchmaking.rs` | rom_hash filter in pairing; new `turn_ready` and `peer_relay` handlers; mints TURN credentials per match if configured |
| `src/main.rs` | Two new routes added: `/match/turn-ready/:session_id` POST, `/match/peer-relay/:session_id` GET |
| `src/turn.rs` | NEW: HMAC-SHA1 REST credential minting for coturn |
| `src/config.rs` | TURN config (turn_server_ip, turn_shared_secret, turn_realm, turn_enabled()) |
| `Cargo.toml` | Added `hmac = "0.12"`, `sha1 = "0.10"`, `base64 = "0.22"` |
| `deploy.sh` | Loads `.env`, validates placeholders, preserves JWT_SECRET, conditionally wires TURN secrets |
| `setup-turn.sh` | Provisions GCE coturn VM, generates shared secret, opens firewall ports |

## Test the address exchange directly (server-side only)

You can verify the new endpoints work without running clients:

```bash
# Get a JWT first (from a logged-in client's token cache)
JWT=eyJ...
BASE=https://<your-service>.run.app

# Force a queue + match (need 2 sessions to actually pair, but you can test the endpoints)
SID=<session_id_from_LFG_response>

# Post a fake relayed address
curl -X POST -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/json" \
  -d '{"relayed_addr":"1.2.3.4:55555"}' \
  $BASE/match/turn-ready/$SID

# Poll for peer's relay
curl -H "Authorization: Bearer $JWT" \
  $BASE/match/peer-relay/$SID
```

## Common deploy issues + fixes

| Symptom | Cause | Fix |
|---|---|---|
| "value 'YOUR_DISCORD_CLIENT_ID' is not snowflake" | Old `deploy.sh` had hardcoded placeholders | Fixed — current script reads `.env` |
| Token still rejected after deploy | JWT_SECRET got rotated, invalidated cached client tokens | Clients re-OAuth (auto on 401). Or restore JWT_SECRET from a previous secret version. |
| Bad syntax for dict arg (commas in metadata) | `--metadata=startup-script=...` doesn't handle commas | Fixed in setup-turn.sh — uses `--metadata-from-file` |
| `--metadata-from-file` unknown | Old gcloud version | `gcloud components update` |
| Cloud Run revision deploys but old code runs | Container image cached, didn't actually rebuild | `gcloud builds submit` should always push fresh — verify image digest matches in revision list |
| coturn VM exists but not running | Debian package ships disabled | startup script flips `TURNSERVER_ENABLED=1` in `/etc/default/coturn` |

## Open architectural questions

- Should we move queue state to Redis to allow horizontal scaling? Current 2-instance ceiling is a soft limit; we won't hit it for a long time but it's there.
- Should `app_version` matching allow patch-level tolerance (e.g. 0.2.0 with 0.2.1)? Currently exact match only.
- Should we add a `/match/heartbeat` endpoint clients ping every N seconds, with server auto-cancelling stale entries? Cleaner than a TTL sweep.
- Is HMAC-SHA1 REST auth still appropriate, or should we move to short-lived TURN credentials issued via OAuth? (Mostly relevant if we need authenticated TURN logging per-user.)