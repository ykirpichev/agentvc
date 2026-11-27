# Hosted review UI (`agentvc-cloud`)

The same review UI that `agentvcd` serves on localhost, at a stable HTTPS URL
you can open from a phone, the Claude app or another computer. Plan and cost
comparison: [hosted service plan](https://claude.ai/artifact/RErWjUobX5k4a7YkuesjhU).

**Status: code only. Nothing is deployed and no cloud resources exist yet.**

## How it works

```
browser ──HTTPS──> Caddy ──> agentvc-cloud (VM) <──WebSocket── agentvcd --cloud (your Mac)
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

To rotate a token: make a new one on the home page, write it to the token
file, then revoke the old one. The daemon is refused, reads the file again
and reconnects with the new token within seconds; no restart. Revoking
forgets the spaces the old token connected, so they come back under new
links.

## Deploying to GCP (always-free e2-micro)

`deploy/gcp/deploy.sh` does it with `gcloud` alone, signed in as someone who
can create projects and has a billing account (GCP requires one even for the
free tier). No Docker and no domain needed: Cloud Build builds the image, and
the public name is `<ip>.sslip.io` with a Let's Encrypt certificate from Caddy
(pass `DOMAIN=` to use your own name; point its A record at the IP first).

To move to another name later, for example a free `<name>.duckdns.org` set to
the static IP: change the GitHub OAuth app's homepage and callback URL, then
`DOMAIN=<name>.duckdns.org deploy/gcp/deploy.sh up`, and restart each
`agentvcd --cloud` with the new URL. Caddy gets the new certificate itself.

```sh
deploy/gcp/deploy.sh setup    # new project, billing, APIs, registry, static IP, firewall
deploy/gcp/deploy.sh build    # Cloud Build, about 10 minutes
# create a GitHub OAuth app with the callback URL that setup printed, then:
GITHUB_CLIENT_ID=... GITHUB_CLIENT_SECRET=... deploy/gcp/deploy.sh up
deploy/gcp/deploy.sh status
```

What it creates in project `agentvc-cloud-<random>`: an e2-micro VM
(Debian 12, 30 GB standard disk) running `docker-compose.yml` (the service plus
Caddy), a static external IP, firewall rules for 80/443 and SSH through IAP
only, an Artifact Registry repository, and a service account that can only
pull images and write logs. Secrets travel as instance metadata into
`/etc/agentvc-cloud.env` (mode 600). Running `up` again with a new `build`
updates the VM and restarts it.

Cost: the VM, disk and Cloud Build minutes are in the free tier. A static
external IPv4 may be billed (about $3 a month) depending on your account; the
Artifact Registry stays under its free 0.5 GB if old images are deleted.

SSH: `gcloud compute ssh agentvc-cloud --zone us-central1-a --tunnel-through-iap --project <project>`.

`docker-compose.tunnel.yml` is the alternative without open ports, through a
Cloudflare Tunnel (needs a domain on Cloudflare).

### Elsewhere

The container needs only the env vars above and a volume at `/data`:

- **Nebius / any VM:** copy `docker-compose.yml` and `/etc/agentvc-cloud.env`, run `docker compose up -d`.
- **Fly.io:** `fly launch --image ghcr.io/ykirpichev/agentvc-cloud`, a 1 GB volume at `/data`, secrets for the env vars; Fly terminates TLS, so skip `cloudflared`.
