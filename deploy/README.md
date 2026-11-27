# Hosted review UI (`agentvc-cloud`)

The same review UI that `agentvcd` serves on localhost, at a stable HTTPS URL
you can open from a phone, the Claude app or another computer. Plan and cost
comparison: [hosted service plan](https://claude.ai/artifact/RErWjUobX5k4a7YkuesjhU).

**Status: code only. Nothing is deployed and no cloud resources exist yet.**

## How it works

```
browser ──HTTPS──> Cloudflare ──tunnel──> agentvc-cloud (VM) <──WebSocket── agentvcd --cloud (your Mac)
```

- `agentvcd --cloud <url>` dials **out** to the service over one WebSocket and
  keeps it open. Nothing on your machine listens publicly.
- The hosted UI lives at `/s/<slug>/`. Its API calls (`/s/<slug>/api/...`) are
  relayed down that socket, run against the daemon's own API, and the answer
  comes back. Live events stream up the same socket.
- The service stores only: GitHub logins on the allow list, session and
  machine-token **hashes**, and the list of connected spaces (name, workdir,
  last seen). Code, diffs and events pass through and are never written down.
- No daemon connected: the UI shows the space as offline (`503 machine_offline`).

### Security

- People sign in with GitHub OAuth (no scopes; the token is used once to read
  the login and dropped). Only logins in `AGENTVC_CLOUD_ALLOW_USERS` get in.
- Machines authenticate with a token made on the home page; revoking it
  disconnects that machine at once.
- Every write from a browser (review actions, forms, the events WebSocket)
  must carry the service's own `Origin`, so other sites cannot use your
  session. Cookies are `HttpOnly`, `SameSite=Lax`, `Secure` over HTTPS.
- `agentvcd --cloud-read-only` lets the hosted UI look but not act.
- The daemon relays only `/api/` calls, never files from disk outside its API.

## Try it locally

```sh
(cd ui && npm ci && npm run build)
cargo build -p agentvc-cloud -p agentvcd
target/debug/agentvc-cloud --public-url http://127.0.0.1:8080 --dev-user me
# open http://127.0.0.1:8080, "Dev sign-in", make a machine token, then in a repo:
AGENTVC_CLOUD_TOKEN=avc_... target/debug/agentvcd --cloud http://127.0.0.1:8080
```

`--dev-user` signs anyone in as that login, so the service refuses it unless it
listens on loopback.

## Configuration

All flags have environment variables, so one image runs anywhere.

| Variable | Meaning |
| --- | --- |
| `AGENTVC_CLOUD_PUBLIC_URL` | Where people reach it, e.g. `https://agentvc.example.com` (required) |
| `AGENTVC_CLOUD_ALLOW_USERS` | GitHub logins allowed to sign in, comma-separated (required) |
| `GITHUB_CLIENT_ID`, `GITHUB_CLIENT_SECRET` | GitHub OAuth app; callback `<public url>/auth/github/callback` |
| `AGENTVC_CLOUD_LISTEN` | Default `127.0.0.1:8080`; `0.0.0.0:8080` in the image |
| `AGENTVC_CLOUD_DATA` | Database directory; `/data` in the image |
| `AGENTVC_CLOUD_BACKUP_HOURS` | Database copy into `DATA/backups` every N hours, last 7 kept (default 24, 0 off) |

On the machine side: `agentvcd --cloud URL` (or `AGENTVC_CLOUD_URL`) with the
token in `AGENTVC_CLOUD_TOKEN` or `--cloud-token-file`.

## Deploying (GCP always-free e2-micro + Cloudflare Tunnel)

What you need, all free:

1. A **GCP project** with billing enabled (required even for the free tier) and
   the Compute Engine API on. The e2-micro must be in `us-west1`,
   `us-central1` or `us-east1` to be free.
2. A **domain on Cloudflare** (free plan) and a **Cloudflare Tunnel** (Zero
   Trust > Networks > Tunnels) whose public hostname points at
   `http://agentvc-cloud:8080`. Copy its token.
3. A **GitHub OAuth app** with callback `https://<host>/auth/github/callback`.
4. The image: run the **Cloud image** workflow (Actions > Cloud image > Run
   workflow, with "push" on) to publish `ghcr.io/ykirpichev/agentvc-cloud`,
   then make the package public or give the VM a pull token.

Then:

```sh
cd deploy/gcp
cat > secrets.tfvars <<'TF'
env_file = <<ENV
AGENTVC_CLOUD_PUBLIC_URL=https://agentvc.example.com
AGENTVC_CLOUD_ALLOW_USERS=ykirpichev
GITHUB_CLIENT_ID=...
GITHUB_CLIENT_SECRET=...
TUNNEL_TOKEN=...
ENV
TF
terraform init
terraform apply -var project=YOUR_PROJECT -var-file=secrets.tfvars
```

The VM installs Docker on first boot, writes `/etc/agentvc-cloud.env` from
instance metadata and starts `deploy/docker-compose.yml` (service plus
`cloudflared`). No inbound port is open except SSH through IAP. Updating: push
a new image, then `sudo docker-compose -f /opt/agentvc-cloud/docker-compose.yml pull && sudo docker-compose -f /opt/agentvc-cloud/docker-compose.yml up -d`
on the VM, or reboot it.

Before applying, check two things that change: whether an in-use external IPv4
on a free-tier VM is billed in your account, and the free-tier regions.

### Elsewhere

The container needs only the env vars above and a volume at `/data`:

- **Nebius / any VM:** copy `docker-compose.yml` and `/etc/agentvc-cloud.env`, run `docker compose up -d`.
- **Fly.io:** `fly launch --image ghcr.io/ykirpichev/agentvc-cloud`, a 1 GB volume at `/data`, secrets for the env vars; Fly terminates TLS, so skip `cloudflared`.
