#!/bin/bash
# Runs on every boot of the agentvc-cloud VM (as root). Idempotent:
# deploy/gcp/deploy.sh up changes the instance metadata and resets the VM.
set -euo pipefail

md() {
  curl -fsS -H 'Metadata-Flavor: Google' \
    "http://metadata.google.internal/computeMetadata/v1/instance/attributes/$1" || true
}

if ! command -v docker >/dev/null; then
  apt-get update
  apt-get install -y docker.io docker-compose
  systemctl enable --now docker
fi

# 1 GB of RAM: a swap file keeps image pulls from running out of memory.
if [ ! -f /swapfile ]; then
  fallocate -l 1G /swapfile && chmod 600 /swapfile && mkswap /swapfile
  echo '/swapfile none swap sw 0 0' >> /etc/fstab
fi
swapon -a || true

umask 077
md agentvc-env > /etc/agentvc-cloud.env
umask 022
mkdir -p /opt/agentvc-cloud /var/lib/agentvc-cloud /var/lib/caddy
# The image runs as distroless "nonroot" (uid 65532).
chown 65532:65532 /var/lib/agentvc-cloud
md agentvc-compose > /opt/agentvc-cloud/docker-compose.yml
md agentvc-caddyfile > /opt/agentvc-cloud/Caddyfile
image=$(md agentvc-image)
echo "AGENTVC_CLOUD_IMAGE=$image" > /opt/agentvc-cloud/.env

# Pull from Artifact Registry with the VM's service account.
registry=${image%%/*}
if [[ "$registry" == *-docker.pkg.dev ]]; then
  token=$(curl -fsS -H 'Metadata-Flavor: Google' \
    http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token |
    sed -E 's/.*"access_token":"([^"]+)".*/\1/')
  echo "$token" | docker login -u oauth2accesstoken --password-stdin "https://$registry"
fi

cd /opt/agentvc-cloud
docker-compose pull
docker-compose up -d --remove-orphans
