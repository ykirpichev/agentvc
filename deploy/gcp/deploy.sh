#!/usr/bin/env bash
# Deploy agentvc-cloud to its own GCP project on an always-free e2-micro VM,
# with HTTPS from Caddy + Let's Encrypt. Only needs `gcloud`, signed in as
# someone who can create projects and link a billing account. No Docker or
# domain needed: the image is built in Cloud Build, and the public name is
# <ip>.sslip.io unless you pass DOMAIN.
#
#   deploy/gcp/deploy.sh setup     project, billing, APIs, registry, static IP, firewall
#   deploy/gcp/deploy.sh build     build the image in Cloud Build
#   GITHUB_CLIENT_ID=... GITHUB_CLIENT_SECRET=... deploy/gcp/deploy.sh up
#                                  create or update the VM and start the service
#   deploy/gcp/deploy.sh status    where it is and whether it answers
#
# Settings (env): PROJECT (default agentvc-cloud-<random>, saved after setup),
# BILLING_ACCOUNT (default: your only open one), REGION (us-central1),
# ZONE (us-central1-a), DOMAIN, ALLOW_USERS (ykirpichev).
# State lives in deploy/gcp/.state (not committed).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
state="$here/.state"
# Settings given on the command line win over the saved state, so
# `DOMAIN=new.name deploy.sh up` moves the service to a new name.
given_domain=${DOMAIN:-}
[ -f "$state" ] && . "$state"
DOMAIN=${given_domain:-${DOMAIN:-}}
# GITHUB_CLIENT_ID / GITHUB_CLIENT_SECRET can live here (chmod 600, not
# committed) instead of the command line.
secrets="$here/.secrets"
[ -f "$secrets" ] && . "$secrets"

REGION=${REGION:-us-central1}
ZONE=${ZONE:-us-central1-a}
ALLOW_USERS=${ALLOW_USERS:-ykirpichev}
VM=agentvc-cloud
REPO=agentvc
SA_NAME=agentvc-cloud-vm

save() {
  cat > "$state" <<STATE
PROJECT=$PROJECT
REGION=$REGION
ZONE=$ZONE
IP=${IP:-}
DOMAIN=${DOMAIN:-}
IMAGE=${IMAGE:-}
STATE
}

need_project() {
  if [ -z "${PROJECT:-}" ]; then
    echo "no project yet: run '$0 setup' first" >&2
    exit 1
  fi
}

g() { gcloud --project "$PROJECT" --quiet "$@"; }

# Retry a command a few times: new service accounts take a moment to appear.
retry() {
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    "$@" && return 0
    sleep 6
  done
  "$@"
}

setup() {
  if [ -z "${PROJECT:-}" ]; then
    # od, not tr | head: under pipefail, head closing the pipe kills tr (SIGPIPE).
    PROJECT="agentvc-cloud-$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')"
  fi
  if ! gcloud projects describe "$PROJECT" >/dev/null 2>&1; then
    echo "creating project $PROJECT"
    gcloud projects create "$PROJECT" --name="agentvc cloud" --quiet
  fi
  save
  if [ -z "${BILLING_ACCOUNT:-}" ]; then
    accounts=$(gcloud billing accounts list --filter=open=true --format='value(name)')
    if [ "$(printf '%s\n' "$accounts" | grep -c .)" != 1 ]; then
      echo "set BILLING_ACCOUNT to one of these open billing accounts:" >&2
      gcloud billing accounts list --filter=open=true >&2
      exit 1
    fi
    BILLING_ACCOUNT=${accounts##*/}
  fi
  echo "linking billing account $BILLING_ACCOUNT (the e2-micro, its disk and Cloud Build stay in the free tier)"
  gcloud billing projects link "$PROJECT" --billing-account="$BILLING_ACCOUNT" --quiet >/dev/null
  echo "enabling APIs"
  g services enable compute.googleapis.com artifactregistry.googleapis.com cloudbuild.googleapis.com iap.googleapis.com
  if ! g artifacts repositories describe "$REPO" --location="$REGION" >/dev/null 2>&1; then
    g artifacts repositories create "$REPO" --repository-format=docker --location="$REGION" \
      --description="agentvc-cloud images"
  fi
  sa="$SA_NAME@$PROJECT.iam.gserviceaccount.com"
  if ! g iam service-accounts describe "$sa" >/dev/null 2>&1; then
    g iam service-accounts create "$SA_NAME" --display-name="agentvc-cloud VM"
  fi
  for role in roles/artifactregistry.reader roles/logging.logWriter; do
    retry g projects add-iam-policy-binding "$PROJECT" --member="serviceAccount:$sa" --role="$role" \
      --condition=None >/dev/null
  done
  # Cloud Build runs as the Compute Engine default service account in new
  # projects; let it push images and write build logs.
  number=$(gcloud projects describe "$PROJECT" --format='value(projectNumber)')
  for role in roles/artifactregistry.writer roles/logging.logWriter roles/storage.objectViewer; do
    retry g projects add-iam-policy-binding "$PROJECT" \
      --member="serviceAccount:$number-compute@developer.gserviceaccount.com" --role="$role" \
      --condition=None >/dev/null
  done
  if ! g compute addresses describe "$VM" --region="$REGION" >/dev/null 2>&1; then
    g compute addresses create "$VM" --region="$REGION" --network-tier=STANDARD
  fi
  IP=$(g compute addresses describe "$VM" --region="$REGION" --format='value(address)')
  DOMAIN=${DOMAIN:-${IP//./-}.sslip.io}
  if ! g compute firewall-rules describe agentvc-cloud-web >/dev/null 2>&1; then
    g compute firewall-rules create agentvc-cloud-web --network=default --direction=INGRESS \
      --allow=tcp:80,tcp:443 --source-ranges=0.0.0.0/0 --target-tags=agentvc-cloud
  fi
  if ! g compute firewall-rules describe agentvc-cloud-iap-ssh >/dev/null 2>&1; then
    g compute firewall-rules create agentvc-cloud-iap-ssh --network=default --direction=INGRESS \
      --allow=tcp:22 --source-ranges=35.235.240.0/20 --target-tags=agentvc-cloud
  fi
  save
  cat <<DONE

Project:   $PROJECT
Public URL: https://$DOMAIN

Next: create a GitHub OAuth app at https://github.com/settings/applications/new
  Homepage URL:               https://$DOMAIN
  Authorization callback URL: https://$DOMAIN/auth/github/callback
then put GITHUB_CLIENT_ID=... and GITHUB_CLIENT_SECRET=... in $secrets (chmod 600)
and run: $0 build && $0 up
DONE
}

build() {
  need_project
  IMAGE="$REGION-docker.pkg.dev/$PROJECT/$REPO/agentvc-cloud:$(git -C "$root" rev-parse --short HEAD)"
  echo "building $IMAGE in Cloud Build (about 10 minutes)"
  (cd "$root" && g builds submit --config=deploy/gcp/cloudbuild.yaml --substitutions=_IMAGE="$IMAGE" .)
  save
}

up() {
  need_project
  : "${IMAGE:?run '$0 build' first}"
  : "${DOMAIN:?run '$0 setup' first}"
  if [ -z "${GITHUB_CLIENT_ID:-}" ] || [ -z "${GITHUB_CLIENT_SECRET:-}" ]; then
    echo "set GITHUB_CLIENT_ID and GITHUB_CLIENT_SECRET (the GitHub OAuth app from '$0 setup')" >&2
    exit 1
  fi
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  umask 077
  cat > "$tmp/env" <<ENV
AGENTVC_DOMAIN=$DOMAIN
AGENTVC_CLOUD_PUBLIC_URL=https://$DOMAIN
AGENTVC_CLOUD_ALLOW_USERS=$ALLOW_USERS
GITHUB_CLIENT_ID=$GITHUB_CLIENT_ID
GITHUB_CLIENT_SECRET=$GITHUB_CLIENT_SECRET
ENV
  printf '%s' "$IMAGE" > "$tmp/image"
  files="agentvc-env=$tmp/env,agentvc-image=$tmp/image,agentvc-compose=$root/deploy/docker-compose.yml,agentvc-caddyfile=$root/deploy/Caddyfile,startup-script=$here/startup.sh"
  sa="$SA_NAME@$PROJECT.iam.gserviceaccount.com"
  if g compute instances describe "$VM" --zone="$ZONE" >/dev/null 2>&1; then
    echo "updating $VM and restarting it"
    g compute instances add-metadata "$VM" --zone="$ZONE" --metadata-from-file="$files"
    g compute instances reset "$VM" --zone="$ZONE"
  else
    echo "creating $VM (e2-micro, free tier)"
    g compute instances create "$VM" --zone="$ZONE" --machine-type=e2-micro \
      --image-family=debian-12 --image-project=debian-cloud \
      --boot-disk-size=30GB --boot-disk-type=pd-standard \
      --address="$IP" --network-tier=STANDARD --tags=agentvc-cloud \
      --service-account="$sa" --scopes=cloud-platform \
      --metadata=enable-oslogin=TRUE --metadata-from-file="$files"
  fi
  save
  echo "started; the first boot installs Docker and gets a certificate (a few minutes)."
  echo "check with: $0 status"
}

status() {
  need_project
  echo "project $PROJECT, https://${DOMAIN:-?}"
  g compute instances describe "$VM" --zone="$ZONE" --format='value(status)' 2>/dev/null || echo "no VM yet"
  if curl -fsS --max-time 10 "https://$DOMAIN/healthz"; then echo " (healthy)"; else echo "not answering yet"; fi
}

case "${1:-}" in
  setup | build | up | status) "$1" ;;
  *)
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
