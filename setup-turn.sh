#!/bin/bash
# setup-turn.sh — Provision a GCE e2-micro VM running coturn with REST API auth

set -euo pipefail

# Load .env if present so settings stay in one place
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )"
if [ -f "${SCRIPT_DIR}/.env" ]; then
  set -a
  # shellcheck disable=SC1091
  source "${SCRIPT_DIR}/.env"
  set +a
fi

PROJECT_ID="${PROJECT_ID:-your-gcp-project}"
ZONE="${ZONE:-us-central1-a}"
REGION="${REGION:-us-central1}"
SERVICE_NAME="${SERVICE_NAME:-signaling-server}"
VM_NAME="${VM_NAME:-turn-server}"
TURN_REALM="${TURN_REALM:-example.com}"
TURN_SHARED_SECRET_NAME="TURN_SHARED_SECRET"

echo "================================================================"
echo "Setting up coturn TURN server on GCE"
echo "Project: ${PROJECT_ID} | Zone: ${ZONE} | VM: ${VM_NAME}"
echo "================================================================"
echo ""

# ── 0. Generate and store the shared secret ──────────────────────────────────
echo "[0/5] Generating shared secret..."

# Reuse existing secret if it exists (so we don't invalidate live matches)
if gcloud secrets describe "${TURN_SHARED_SECRET_NAME}" --project="${PROJECT_ID}" &>/dev/null; then
  echo "  Reading existing secret"
  SHARED_SECRET=$(gcloud secrets versions access latest --secret="${TURN_SHARED_SECRET_NAME}" --project="${PROJECT_ID}")
else
  echo "  Creating new secret"
  SHARED_SECRET=$(openssl rand -hex 32)
  echo -n "${SHARED_SECRET}" | gcloud secrets create "${TURN_SHARED_SECRET_NAME}" --data-file=- --project="${PROJECT_ID}" >/dev/null
fi

PROJECT_NUMBER=$(gcloud projects describe "${PROJECT_ID}" --format="value(projectNumber)")
COMPUTE_SA="${PROJECT_NUMBER}-compute@developer.gserviceaccount.com"
gcloud secrets add-iam-policy-binding "${TURN_SHARED_SECRET_NAME}" \
  --member="serviceAccount:${COMPUTE_SA}" \
  --role="roles/secretmanager.secretAccessor" \
  --project="${PROJECT_ID}" \
  --condition=None \
  >/dev/null 2>&1 || true
echo "  ✓ Secret ready"
echo ""

# ── 1. Create firewall rules ──────────────────────────────────────────────────
echo "[1/5] Creating firewall rules..."

gcloud compute firewall-rules create allow-turn-udp \
  --project="${PROJECT_ID}" \
  --direction=INGRESS \
  --action=ALLOW \
  --rules=udp:3478,udp:5349 \
  --source-ranges=0.0.0.0/0 \
  --target-tags=turn-server \
  &>/dev/null || echo "    (allow-turn-udp exists)"

gcloud compute firewall-rules create allow-turn-tcp \
  --project="${PROJECT_ID}" \
  --direction=INGRESS \
  --action=ALLOW \
  --rules=tcp:3478,tcp:5349 \
  --source-ranges=0.0.0.0/0 \
  --target-tags=turn-server \
  &>/dev/null || echo "    (allow-turn-tcp exists)"

gcloud compute firewall-rules create allow-turn-relay \
  --project="${PROJECT_ID}" \
  --direction=INGRESS \
  --action=ALLOW \
  --rules=udp:49152-49251 \
  --source-ranges=0.0.0.0/0 \
  --target-tags=turn-server \
  &>/dev/null || echo "    (allow-turn-relay exists)"

echo "  ✓ Firewall rules ready"
echo ""

# ── 2. Write startup script to a temp file ────────────────────────────────────
# Using --metadata-from-file avoids comma-parsing issues in --metadata
echo "[2/5] Writing VM startup script to temp file..."

STARTUP_FILE=$(mktemp /tmp/turn-startup-XXXXXX.sh)
cat > "${STARTUP_FILE}" <<STARTUP_EOF
#!/bin/bash
set -e
exec > >(tee /var/log/turn-setup.log) 2>&1

apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y coturn

EXTERNAL_IP=\$(curl -s -H "Metadata-Flavor: Google" http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip)

cat > /etc/turnserver.conf <<CONF
listening-port=3478
tls-listening-port=5349
listening-ip=0.0.0.0

min-port=49152
max-port=49251

external-ip=\${EXTERNAL_IP}

use-auth-secret
static-auth-secret=${SHARED_SECRET}

realm=${TURN_REALM}

no-multicast-peers
no-cli
no-tlsv1
no-tlsv1_1

user-quota=12
total-quota=1200

log-file=/var/log/turnserver.log
pidfile=/var/run/turnserver.pid
simple-log
CONF

# coturn ships with TURNSERVER_ENABLED=0 by default on Debian — enable it
sed -i 's/#TURNSERVER_ENABLED=1/TURNSERVER_ENABLED=1/' /etc/default/coturn 2>/dev/null || echo "TURNSERVER_ENABLED=1" > /etc/default/coturn

systemctl enable coturn
systemctl restart coturn
echo "coturn started on \${EXTERNAL_IP}"
STARTUP_EOF

echo "  ✓ Startup script at ${STARTUP_FILE}"
echo ""

# ── 3. Create the VM ──────────────────────────────────────────────────────────
echo "[3/5] Creating e2-micro VM..."

if gcloud compute instances describe "${VM_NAME}" --zone="${ZONE}" --project="${PROJECT_ID}" &>/dev/null; then
  echo "  VM already exists, skipping creation"
  echo "  To recreate: gcloud compute instances delete ${VM_NAME} --zone=${ZONE}"
else
  gcloud compute instances create "${VM_NAME}" \
    --project="${PROJECT_ID}" \
    --zone="${ZONE}" \
    --machine-type=e2-micro \
    --image-family=debian-12 \
    --image-project=debian-cloud \
    --boot-disk-size=10GB \
    --tags=turn-server \
    --metadata-from-file=startup-script="${STARTUP_FILE}"
  echo "  ✓ VM created"
fi

rm -f "${STARTUP_FILE}"
echo ""

# ── 4. Fetch the VM's public IP ───────────────────────────────────────────────
echo "[4/5] Fetching VM public IP..."
sleep 3
TURN_IP=$(gcloud compute instances describe "${VM_NAME}" \
  --zone="${ZONE}" \
  --project="${PROJECT_ID}" \
  --format="value(networkInterfaces[0].accessConfigs[0].natIP)")
echo "  ✓ TURN server public IP: ${TURN_IP}"
echo ""

# Store the IP
if gcloud secrets describe "TURN_SERVER_IP" --project="${PROJECT_ID}" &>/dev/null; then
  echo -n "${TURN_IP}" | gcloud secrets versions add "TURN_SERVER_IP" --data-file=- --project="${PROJECT_ID}" >/dev/null
else
  echo -n "${TURN_IP}" | gcloud secrets create "TURN_SERVER_IP" --data-file=- --project="${PROJECT_ID}" >/dev/null
fi
gcloud secrets add-iam-policy-binding "TURN_SERVER_IP" \
  --member="serviceAccount:${COMPUTE_SA}" \
  --role="roles/secretmanager.secretAccessor" \
  --project="${PROJECT_ID}" \
  --condition=None \
  >/dev/null 2>&1 || true

# ── 5. Update Cloud Run ───────────────────────────────────────────────────────
echo "[5/5] Updating Cloud Run service with TURN credentials..."

# Only attempt this if the signaling service exists (first deploy may not have run yet)
if gcloud run services describe "${SERVICE_NAME}" --region="${REGION}" --project="${PROJECT_ID}" &>/dev/null; then
  gcloud run services update "${SERVICE_NAME}" \
    --region="${REGION}" \
    --project="${PROJECT_ID}" \
    --update-secrets="TURN_SHARED_SECRET=TURN_SHARED_SECRET:latest,TURN_SERVER_IP=TURN_SERVER_IP:latest" \
    >/dev/null
  echo "  ✓ Cloud Run updated"
else
  echo "  ⚠ ${SERVICE_NAME} service not found, skipping Cloud Run update"
  echo "    Deploy the signaling server (bash deploy.sh) first, then re-run this script"
fi
echo ""

echo "================================================================"
echo "✅ TURN server deployed"
echo "================================================================"
echo ""
echo "TURN URI:     turn:${TURN_IP}:3478"
echo "Realm:        ${TURN_REALM}"
echo "Auth method:  REST API (time-limited credentials)"
echo ""
echo "Wait 60-90 seconds for coturn to finish installing, then verify:"
echo "  gcloud compute ssh ${VM_NAME} --zone=${ZONE} --command='sudo systemctl status coturn'"
echo "  gcloud compute ssh ${VM_NAME} --zone=${ZONE} --command='sudo tail /var/log/turn-setup.log'"
echo ""
echo "Next: redeploy the signaling server (bash deploy.sh) so it picks up TURN secrets"