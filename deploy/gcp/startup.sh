#!/bin/bash
# Runs on every boot of the agentvc-cloud VM (as root). Idempotent.
set -euo pipefail

md() {
  curl -fsS -H 'Metadata-Flavor: Google' \
    "http://metadata.google.internal/computeMetadata/v1/instance/attributes/$1" || true
}

if ! command -v docker >/dev/null; then
  apt-get update
  apt-get install -y docker.io docker-compose sqlite3
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
mkdir -p /opt/agentvc-cloud /var/lib/agentvc-cloud
# The image runs as distroless "nonroot" (uid 65532).
chown 65532:65532 /var/lib/agentvc-cloud
md agentvc-compose > /opt/agentvc-cloud/docker-compose.yml
echo "AGENTVC_CLOUD_IMAGE=$(md agentvc-image)" > /opt/agentvc-cloud/.env

cd /opt/agentvc-cloud
docker-compose pull
docker-compose up -d

# Nightly: copy the newest database backup to GCS, if a bucket is set.
bucket=$(md agentvc-backup-bucket)
if [ -n "$bucket" ]; then
  cat > /etc/cron.daily/agentvc-cloud-backup <<CRON
#!/bin/sh
latest=\$(ls -1t /var/lib/agentvc-cloud/backups/*.db 2>/dev/null | head -1)
[ -n "\$latest" ] && gcloud storage cp "\$latest" "gs://$bucket/" --quiet
CRON
  chmod 755 /etc/cron.daily/agentvc-cloud-backup
fi
