#!/bin/bash
# deploy.sh — signaling server → Cloud Run
#
# Reads Discord credentials from .env in the same directory as this script.
# That file should be gitignored — it holds your real secrets.
#
# Required .env contents:
#   DISCORD_CLIENT_ID=...
#   DISCORD_CLIENT_SECRET=...
#   DISCORD_WEBHOOK_URL=...           (optional — leave blank to skip)
#   JWT_SECRET=...                    (optional — auto-generated if missing)
#   STATS_SERVICE_URL=...             (optional — stats service /results URL)
#   STATS_API_KEY=...                 (optional — matches STATS_API_KEY on stats service)
#   GITHUB_ISSUES_REPO=owner/repo     (optional — auto-open/update incident issues)
#   GITHUB_ISSUES_TOKEN=ghp_...       (optional — fine-grained token with Issues write)

set -euo pipefail

# ── Load .env ─────────────────────────────────────────────────────────────────
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )"
ENV_FILE="${SCRIPT_DIR}/.env"

if [ ! -f "${ENV_FILE}" ]; then
  echo "❌ ${ENV_FILE} not found."
  echo "   Copy .env.example to .env and fill in your values."
  exit 1
fi

set -a
# shellcheck disable=SC1090
source "${ENV_FILE}"
set +a

# ── Config (.env overrides these; otherwise sane defaults) ────────────────────
PROJECT_ID="${PROJECT_ID:-your-gcp-project}"
REGION="${REGION:-us-central1}"
SERVICE_NAME="${SERVICE_NAME:-signaling-server}"
IMAGE="gcr.io/${PROJECT_ID}/${SERVICE_NAME}"

if [ -z "${DISCORD_CLIENT_ID:-}" ] || [ "${DISCORD_CLIENT_ID}" = "YOUR_DISCORD_CLIENT_ID" ]; then
  echo "❌ DISCORD_CLIENT_ID missing or placeholder in ${ENV_FILE}"
  exit 1
fi
if [ -z "${DISCORD_CLIENT_SECRET:-}" ] || [ "${DISCORD_CLIENT_SECRET}" = "YOUR_DISCORD_CLIENT_SECRET" ]; then
  echo "❌ DISCORD_CLIENT_SECRET missing or placeholder in ${ENV_FILE}"
  exit 1
fi
DISCORD_WEBHOOK_URL="${DISCORD_WEBHOOK_URL:-}"
STATS_SERVICE_URL="${STATS_SERVICE_URL:-}"
STATS_API_KEY="${STATS_API_KEY:-}"
GITHUB_ISSUES_REPO="${GITHUB_ISSUES_REPO:-}"
GITHUB_ISSUES_TOKEN="${GITHUB_ISSUES_TOKEN:-}"

echo "================================================================"
echo "Project: ${PROJECT_ID} | Region: ${REGION} | Service: ${SERVICE_NAME}"
echo "================================================================"
echo "Loaded credentials from ${ENV_FILE}"
echo "  DISCORD_CLIENT_ID: ${DISCORD_CLIENT_ID:0:6}..."
echo ""

# ── 0. Preflight checks ───────────────────────────────────────────────────────
echo "[0/5] Preflight checks..."

if ! gcloud auth list --filter=status:ACTIVE --format="value(account)" | grep -q .; then
  echo "  ❌ Not authenticated. Run: gcloud auth login"
  exit 1
fi
ACTIVE_ACCOUNT=$(gcloud auth list --filter=status:ACTIVE --format="value(account)")
echo "  ✓ Authenticated as: ${ACTIVE_ACCOUNT}"

if ! gcloud projects describe "${PROJECT_ID}" &>/dev/null; then
  echo "  ❌ Cannot access project '${PROJECT_ID}'."
  exit 1
fi
echo "  ✓ Project accessible"

PROJECT_NUMBER=$(gcloud projects describe "${PROJECT_ID}" --format="value(projectNumber)")
COMPUTE_SA="${PROJECT_NUMBER}-compute@developer.gserviceaccount.com"
echo "  ✓ Project number: ${PROJECT_NUMBER}"
echo "  ✓ Compute SA: ${COMPUTE_SA}"

BILLING_ENABLED=$(gcloud beta billing projects describe "${PROJECT_ID}" \
  --format="value(billingEnabled)" 2>/dev/null || echo "unknown")
if [ "${BILLING_ENABLED}" = "False" ]; then
  echo "  ❌ Billing not enabled."
  exit 1
fi
echo "  ✓ Billing check passed"
echo ""

# ── 1. Enable required GCP APIs ───────────────────────────────────────────────
echo "[1/5] Enabling GCP APIs..."
gcloud services enable \
  run.googleapis.com \
  secretmanager.googleapis.com \
  cloudbuild.googleapis.com \
  containerregistry.googleapis.com \
  --project="${PROJECT_ID}"
echo ""

# ── 2. Create or update secrets ───────────────────────────────────────────────
echo "[2/5] Writing secrets to Secret Manager..."

create_secret() {
  local name=$1
  local value=$2
  if gcloud secrets describe "${name}" --project="${PROJECT_ID}" &>/dev/null; then
    echo "  Updating: ${name}"
    echo -n "${value}" | gcloud secrets versions add "${name}" --data-file=- --project="${PROJECT_ID}" >/dev/null
  else
    echo "  Creating: ${name}"
    echo -n "${value}" | gcloud secrets create "${name}" --data-file=- --project="${PROJECT_ID}" >/dev/null
  fi
}

create_secret "DISCORD_CLIENT_ID"     "${DISCORD_CLIENT_ID}"
create_secret "DISCORD_CLIENT_SECRET" "${DISCORD_CLIENT_SECRET}"

if [ -n "${DISCORD_WEBHOOK_URL}" ]; then
  create_secret "DISCORD_WEBHOOK_URL" "${DISCORD_WEBHOOK_URL}"
else
  create_secret "DISCORD_WEBHOOK_URL" ""
fi

# JWT_SECRET — only generate fresh if it doesn't already exist (preserves existing logins)
if ! gcloud secrets describe "JWT_SECRET" --project="${PROJECT_ID}" &>/dev/null; then
  if [ -n "${JWT_SECRET:-}" ]; then
    create_secret "JWT_SECRET" "${JWT_SECRET}"
  else
    create_secret "JWT_SECRET" "$(openssl rand -base64 48)"
  fi
  echo "  ✓ JWT_SECRET created"
else
  echo "  ✓ JWT_SECRET already exists (preserving existing tokens)"
fi
echo ""

# ── 3. Grant Cloud Run's compute SA access to each secret ─────────────────────
echo "[3/5] Granting secret access to Cloud Run service account..."

for SECRET in DISCORD_CLIENT_ID DISCORD_CLIENT_SECRET DISCORD_WEBHOOK_URL JWT_SECRET; do
  gcloud secrets add-iam-policy-binding "${SECRET}" \
    --member="serviceAccount:${COMPUTE_SA}" \
    --role="roles/secretmanager.secretAccessor" \
    --project="${PROJECT_ID}" \
    --condition=None \
    >/dev/null 2>&1 || echo "    (binding already exists for ${SECRET})"
done
echo "  ✓ Access granted"
echo ""

# ── 3b. Ensure incidents bucket exists & SA has write access ──────────────────
# Created lazily so first-run deploys don't need a separate provisioning step.
# The bucket has a 90-day lifecycle policy that auto-deletes old incidents.
INCIDENTS_BUCKET="${PROJECT_ID}-freeplay-incidents"
if ! gcloud storage buckets describe "gs://${INCIDENTS_BUCKET}" --project="${PROJECT_ID}" &>/dev/null; then
  echo "  Creating incidents bucket: gs://${INCIDENTS_BUCKET}"
  gcloud storage buckets create "gs://${INCIDENTS_BUCKET}" \
    --project="${PROJECT_ID}" --location="${REGION}" --uniform-bucket-level-access >/dev/null
  cat > /tmp/incidents-lifecycle.json <<EOF
{"rule":[{"action":{"type":"Delete"},"condition":{"age":90}}]}
EOF
  gcloud storage buckets update "gs://${INCIDENTS_BUCKET}" \
    --lifecycle-file=/tmp/incidents-lifecycle.json >/dev/null
fi
gcloud storage buckets add-iam-policy-binding "gs://${INCIDENTS_BUCKET}" \
  --member="serviceAccount:${COMPUTE_SA}" \
  --role="roles/storage.objectAdmin" \
  --condition=None \
  >/dev/null 2>&1 || true
echo "  ✓ Incidents bucket: gs://${INCIDENTS_BUCKET}"
echo ""

# ── 4. Build image via Cloud Build ────────────────────────────────────────────
echo "[4/5] Building image on Cloud Build..."
gcloud builds submit --tag "${IMAGE}" --project="${PROJECT_ID}"
echo "  ✓ Image pushed: ${IMAGE}"
echo ""

# ── 5. Deploy to Cloud Run ────────────────────────────────────────────────────
echo "[5/5] Deploying to Cloud Run..."

SECRETS_LIST="DISCORD_CLIENT_ID=DISCORD_CLIENT_ID:latest"
SECRETS_LIST="${SECRETS_LIST},DISCORD_CLIENT_SECRET=DISCORD_CLIENT_SECRET:latest"
SECRETS_LIST="${SECRETS_LIST},DISCORD_WEBHOOK_URL=DISCORD_WEBHOOK_URL:latest"
SECRETS_LIST="${SECRETS_LIST},JWT_SECRET=JWT_SECRET:latest"

# Conditionally include TURN secrets if they're already provisioned
if gcloud secrets describe TURN_SHARED_SECRET --project="${PROJECT_ID}" &>/dev/null; then
  SECRETS_LIST="${SECRETS_LIST},TURN_SHARED_SECRET=TURN_SHARED_SECRET:latest"
fi
if gcloud secrets describe TURN_SERVER_IP --project="${PROJECT_ID}" &>/dev/null; then
  SECRETS_LIST="${SECRETS_LIST},TURN_SERVER_IP=TURN_SERVER_IP:latest"
fi

# Conditionally include stats API key
ENV_VARS=""
if [ -n "${STATS_API_KEY}" ]; then
  create_secret "STATS_API_KEY" "${STATS_API_KEY}"
  gcloud secrets add-iam-policy-binding "STATS_API_KEY" \
    --member="serviceAccount:${COMPUTE_SA}" \
    --role="roles/secretmanager.secretAccessor" \
    --project="${PROJECT_ID}" \
    --condition=None \
    >/dev/null 2>&1 || true
  SECRETS_LIST="${SECRETS_LIST},STATS_API_KEY=STATS_API_KEY:latest"
elif gcloud secrets describe STATS_API_KEY --project="${PROJECT_ID}" &>/dev/null; then
  SECRETS_LIST="${SECRETS_LIST},STATS_API_KEY=STATS_API_KEY:latest"
fi

if [ -n "${GITHUB_ISSUES_TOKEN}" ]; then
  create_secret "GITHUB_ISSUES_TOKEN" "${GITHUB_ISSUES_TOKEN}"
  gcloud secrets add-iam-policy-binding "GITHUB_ISSUES_TOKEN" \
    --member="serviceAccount:${COMPUTE_SA}" \
    --role="roles/secretmanager.secretAccessor" \
    --project="${PROJECT_ID}" \
    --condition=None \
    >/dev/null 2>&1 || true
  SECRETS_LIST="${SECRETS_LIST},GITHUB_ISSUES_TOKEN=GITHUB_ISSUES_TOKEN:latest"
elif gcloud secrets describe GITHUB_ISSUES_TOKEN --project="${PROJECT_ID}" &>/dev/null; then
  SECRETS_LIST="${SECRETS_LIST},GITHUB_ISSUES_TOKEN=GITHUB_ISSUES_TOKEN:latest"
fi

if [ -n "${STATS_SERVICE_URL}" ]; then
  ENV_VARS="STATS_SERVICE_URL=${STATS_SERVICE_URL}"
fi
if [ -n "${GITHUB_ISSUES_REPO}" ]; then
  if [ -n "${ENV_VARS}" ]; then
    ENV_VARS="${ENV_VARS},GITHUB_ISSUES_REPO=${GITHUB_ISSUES_REPO}"
  else
    ENV_VARS="GITHUB_ISSUES_REPO=${GITHUB_ISSUES_REPO}"
  fi
fi

DEPLOY_ARGS=(
  --image="${IMAGE}"
  --platform=managed
  --region="${REGION}"
  --project="${PROJECT_ID}"
  --allow-unauthenticated
  --min-instances=0
  --max-instances=2
  --memory=256Mi
  --cpu=1
  --set-secrets="${SECRETS_LIST}"
)

if [ -n "${ENV_VARS}" ]; then
  DEPLOY_ARGS+=(--set-env-vars="${ENV_VARS}")
fi

gcloud run deploy "${SERVICE_NAME}" "${DEPLOY_ARGS[@]}"

SERVICE_URL=$(gcloud run services describe "${SERVICE_NAME}" \
  --region="${REGION}" \
  --project="${PROJECT_ID}" \
  --format="value(status.url)")

REDIRECT_URI="${SERVICE_URL}/auth/discord/callback"
create_secret "DISCORD_REDIRECT_URI" "${REDIRECT_URI}"
gcloud secrets add-iam-policy-binding "DISCORD_REDIRECT_URI" \
  --member="serviceAccount:${COMPUTE_SA}" \
  --role="roles/secretmanager.secretAccessor" \
  --project="${PROJECT_ID}" \
  --condition=None \
  >/dev/null 2>&1 || true

gcloud run services update "${SERVICE_NAME}" \
  --region="${REGION}" \
  --project="${PROJECT_ID}" \
  --update-secrets="DISCORD_REDIRECT_URI=DISCORD_REDIRECT_URI:latest" \
  >/dev/null

echo ""
echo "================================================================"
echo "✅  Deployed: ${SERVICE_URL}"
echo "================================================================"
echo ""
echo "Next steps:"
echo ""
echo "  1. Confirm this URL is in your Discord app's OAuth2 Redirects:"
echo "     ${REDIRECT_URI}"
echo "     https://discord.com/developers/applications"
echo ""
echo "  2. Confirm your client's SERVER_URL matches:"
echo "     ${SERVICE_URL}"
echo ""
echo "  3. Deploy the stats service (freeplay-stats) if not already done."
echo "     Then set STATS_SERVICE_URL and STATS_API_KEY in .env and redeploy."
echo ""
echo "  4. Test the health endpoint:"
echo "     curl ${SERVICE_URL}/health"
echo ""
echo "  5. Watch live logs:"
echo "     gcloud beta run services logs tail ${SERVICE_NAME} --region=${REGION}"
