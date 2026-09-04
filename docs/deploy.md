# Deploying `attemptdb-server`

The sync server is the same engine as the CLI, run once for many tenants:
one `.attemptdb` directory per tenant under a data directory, a key file of
digests, and an HTTP/1.1 listener. It is a single static binary. This page
is the operator's view; the wire contract is `docs/server-api.md` and RFC
0006 §10.

## Shape of a deployment

```
                 ┌────────── VM (one persistent disk) ──────────┐
 devices ──TLS──►│ caddy ─HTTP─► attemptdb-server ─► /data       │
 web/app ─TLS──►│                 │                 ├ keys.json   │
                 │                 └ SIGHUP reload   └ tenants/<t>/ .attemptdb
                 └───────────────────────────────────────────────┘
```

- **One VM, one disk.** The engine takes an exclusive `flock` per tenant
  database and fsyncs its WAL before acknowledging a batch. That rules out
  serverless runtimes with ephemeral or network-shared filesystems (Cloud
  Run, Lambda): run it where a local disk persists across restarts — a VM
  with an attached volume, or a container with a named volume on one host.
- **TLS in front.** The process speaks plain HTTP on purpose; put Caddy,
  nginx, or the cloud load balancer in front. `deploy/docker-compose.yml`
  runs Caddy with automatic certificates.
- **Stateless apart from `/data`.** Everything the server knows is under
  the data directory; the container image carries no state.

## Run it

```sh
git clone https://github.com/nullarch/attemptdb && cd attemptdb
ATTEMPTDB_ADMIN_TOKEN=$(openssl rand -hex 32) \
ATTEMPTDB_DOMAIN=sync.example.com \
docker compose -f deploy/docker-compose.yml up -d --build
curl https://sync.example.com/v1/health
```

`deploy/Dockerfile` **downloads** the release: the Release workflow builds
`attemptdb-server` (plus the `attempt` CLI for host-side operations) for
the static Linux targets, attests it, and publishes
`attemptdb-server-<version>-<target>.tar.gz`; the image fetches that
archive, checks it against `SHA256SUMS`, and installs it. A deploy is a
download — seconds. `ATTEMPTDB_VERSION` comes in as a build arg
(`deploy/fly-up.sh` passes the version in `Cargo.toml`). For a change
that is not released yet, `deploy/Dockerfile.source` builds from the tree
with the `server` cargo profile (`deploy/fly-up.sh --source`, ~5 minutes on
Fly's builder; the `release` profile's LTO and single codegen unit took
13–22 minutes there for no gain a sync server can feel). The entrypoint
creates an empty `/data/keys.json` on first start. Environment:

| Variable | Default | Meaning |
|---|---|---|
| `ATTEMPTDB_ADMIN_TOKEN` | unset | Enables `/v1/admin/*`. Unset → those routes answer 404. Keep it in the deployment's secret store; it is the only credential that can mint keys. |
| `ATTEMPTDB_CAPTURE_MODE` | `metadata_only` | Ceiling on what any client may persist here. `metadata_only` means no prompt, command, file content or tool output ever reaches this disk, whatever a client sends. Raising it is a privacy decision (RFC 0006 §10.6), not a configuration default. |
| `ATTEMPTDB_BIND` / `ATTEMPTDB_PORT` | `0.0.0.0` / `8787` | Listener inside the container. |
| `ATTEMPTDB_MAX_OPEN` | `256` | How many tenant databases stay resident. This is the memory dial: an open tenant's read cache holds that tenant's whole projected history, so `RSS ≈ 11 MiB + Σ(events × ~4–5 KiB)` over open tenants. Measured (2026-09-02): 3 tenants × 20,000 events = 404 MiB; one tenant of 200,000 ≈ 800 MiB. Lower it on a small machine. |
| `ATTEMPTDB_IDLE_FLUSH_SECS` | `300` | Flush and close a tenant idle this long, returning its memory. |
| `ATTEMPTDB_WEBHOOK_URL` / `ATTEMPTDB_WEBHOOK_SECRET` | unset | Deliver accepted events to the product's endpoint, HMAC-signed, from a durable per-tenant cursor (`docs/server-api.md`, "Outbound webhook"). Both or neither; the secret belongs in the secret store. |
| `ATTEMPTDB_VIEW_WINDOW_DAYS` | `0` (all) | Keep only the last N days of a tenant's events in its resident view; segments before the window are never decoded. Bounds memory per organisation tenant (a 10-person organisation: ~250 MB for 14 days against 5–6 GB for a year). `/v1/status` shows the window; `/v1/events` backfill reads the whole history regardless. |

### Fly.io (what vibemon.dev runs)

`deploy/fly.toml` is one shared-cpu-1x / 1 GB Machine in `iad` with one
1 GB Volume, `auto_stop_machines = "off"` and `min_machines_running = 1`
(never `fly scale count 2`: two machines are two volumes and two silently
diverging databases). `deploy/fly-up.sh` is the idempotent bootstrap —
app, volume, admin-token secret, deploy, certificate, the DNS record to
add, health:

```sh
fly auth login                                              # once, interactive
ATTEMPTDB_ADMIN_TOKEN_FILE=~/.attemptdb-admin-token deploy/fly-up.sh
```

After that, **deploy = release**: `.github/workflows/deploy.yml` runs `fly
deploy` with the published version whenever a release is published (or by
hand with a chosen version), using the `FLY_API_TOKEN` repository secret
(a deploy token for the app: `fly tokens create deploy -a attemptdb-sync`).
The console is at `https://<host>/admin` (docs/server-api.md).

The token file is generated if absent; the product's backend gets the same
value (`vibemon-web`: `ATTEMPTDB_ADMIN_TOKEN`, production only). Then add
the printed `A` record for `sync.vibemon.dev` at the zone (Cloud DNS) and
watch `fly certs check sync.vibemon.dev`. Backups: `fly volumes snapshots
list attemptdb_data` — Fly takes a daily snapshot of every volume (5-day
retention by default; `fly volumes snapshots create` for an ad-hoc one).
Restore is a new volume from the snapshot (`fly volumes create --snapshot-id
…`) attached in place of the old one; verify with `attempt verify --db
/data/tenants/<t>` from `fly ssh console`. Alerts: a failing `/v1/health`
check restarts the Machine; point an external monitor at
`https://sync.vibemon.dev/v1/health` for the case where the whole Machine
is gone.

Without Docker: build with `cargo build --release -p attemptdb-server` and
run `attemptdb-server --bind 127.0.0.1 --data-dir /srv/attemptdb --keys
/srv/attemptdb/keys.json` under systemd, with the admin token in the unit's
environment.

## Keys

Devices authenticate with bearer keys the server mints; the file holds
only SHA-256 digests, so a leaked file is not a leaked credential.

```sh
# The product's backend links a device to a user:
curl -X POST https://sync.example.com/v1/admin/keys \
  -H "Authorization: Bearer $ATTEMPTDB_ADMIN_TOKEN" \
  -d '{"tenant":"org_acme","user_id":"usr_42","scope":"device","label":"kevin laptop"}'
# → {"key":"atk_…", …}   hand the key to the installer; it is shown once.

# A reader key for the web/app backend (may read the tenant, never write):
curl -X POST … -d '{"tenant":"org_acme","scope":"reader","label":"web"}'

# A device leaves: keys revoked, its sessions retracted from every projection.
curl -X DELETE https://sync.example.com/v1/admin/devices/dev_… \
  -H "Authorization: Bearer $ATTEMPTDB_ADMIN_TOKEN"
```

Hand-editing `keys.json` is fine: `attemptdb-server digest <key>` prints the
line, and `kill -HUP <pid>` (or `POST /v1/admin/keys/reload`) re-reads the
file without a restart.

Tenant ids are opaque directory names (`[A-Za-z0-9._-]{1,64}`). Choose
them once — an organisation per tenant, with a personal organisation for a
solo user, is the shape the product uses — because renaming a tenant is a
directory move plus a key-file edit while the server is stopped.

## Backups

The on-disk format is crash-consistent by construction (`docs/storage-format.md`):
every acknowledged event is in an fsynced WAL, segments are immutable and
checksummed, and the manifest is replaced atomically. A **filesystem-level
snapshot of the volume** taken at any instant is therefore a valid backup;
a restore replays the WAL tail on first open. Take them with the disk's
snapshot facility (cloud volume snapshots, ZFS/btrfs, LVM) on a schedule.

Verify a backup without stopping anything — read-only opens take no lock:

```sh
attempt verify --db /mnt/restored/tenants/org_acme
attempt status --db /mnt/restored/tenants/org_acme --json
```

`attempt snapshot export` produces a portable single-file `.atdb`, but it
needs the writer, so on a live server it only works for a tenant the server
does not currently hold open (idle tenants are closed after
`--idle-flush-secs`, default 300). A flush-and-export admin endpoint is on
the list; until then, volume snapshots are the backup.

## Backfill

History collected by the legacy `vibemon-hooks` client can be replayed into
a tenant with `attempt import vibemon-export <export.ndjson> --db
/data/tenants/<tenant>` while that tenant is not open by the server (stop
the server, or wait for the idle sweep). The import is idempotent: event ids
are derived from the source rows, so a re-run stores nothing new. See
`docs/migration/vibemon-hooks.md`.

## Operations

- **Health**: `GET /v1/health` → `{"status":"ok","open_tenants":N,…}`.
- **Upgrade**: stop (the server flushes every open tenant on shutdown — give
  it `stop_grace_period: 30s`), replace the image, start. A new binary opens
  old databases; the format version is checked on open and refused loudly
  if unknown, never rewritten silently.
- **Compaction**: when a tenant is flushed and closed (idle sweep, LRU
  eviction) the server merges its small segments, a few generations per
  close, so long-lived tenants stay at a handful of files without a pause
  on the request path; `--no-compaction` turns it off, `attempt compact
  --db tenants/<t>` does it by hand.
- **Capacity**: `--max-open` (default 256) bounds resident tenant databases
  (LRU, never evicts one with a request in flight); `--idle-flush-secs`
  closes quiet tenants. Metadata-only events cost ~134 bytes each on disk.
  Memory per open tenant is dominated by the memtable (≤ 20 000 events /
  64 MiB) plus the read cache once the read API is in use.
- **Logs**: stderr only; one line per notable event (start, key reload,
  flush failures). Ingest volume is not logged per request — read
  `/v1/health` or the tenant directories.
- **Deleting a tenant**: stop the server (or wait until it is idle and
  closed), `rm -r tenants/<tenant>`, remove its keys. Account deletion is
  that operation plus whatever the product keeps elsewhere.

## What this page does not cover

Object-storage tiering of segments and multiple server nodes over shared
segments are not implemented; the design for them is in RFC 0006 §10 and
TODO.md §13 "Cloud architecture".

## Pairing and the one-line install

`docs/migration/vibemon-install.sh` (and `.ps1`) is the command a product
gives its users: `curl -fsSL https://<product>/install.sh | sh -s -- pair_…`.
The web app mints the token with `POST /v1/admin/pairings` (admin token,
server to server) and shows the command; the installer checks the token
first, installs and initialises, pairs (`attempt sync connect --pair`),
installs hooks, registers the daemon, uploads once, and only then removes a
legacy collector's hooks. Without a token on a never-connected machine it
exits 0 having changed nothing, which is what a legacy client's unattended
auto-update run hits. Flags: `--rate-limit` / `--pair-rate-limit` on the
server (`ATTEMPTDB_RATE_LIMIT`, `ATTEMPTDB_PAIR_RATE_LIMIT`).
