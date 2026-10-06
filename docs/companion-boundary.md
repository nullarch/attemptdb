# Where a companion meets AttemptDB

AttemptDB is the engine. A companion — today VibeMon — is the product a
person talks to: accounts, teams, notifications, a phone. This page lists
every place the two meet **as they are today** and names the document that
is authoritative for each. It changes no behaviour. It exists so that a
change on either side knows what it touches.

**Direction.** The companion depends on AttemptDB; AttemptDB never depends
on the companion. Every local command works without an account or a
network, and nothing in this repository waits on a companion's decision.
Anything that speaks to a person first (a notification, a badge, a
menu-bar line) is built on the companion's side of these surfaces — see
[AGENTS.md](../AGENTS.md) *Scope*.

## The surfaces

| # | Surface | Who calls whom | Authoritative |
|---|---|---|---|
| 1 | **Install with pairing** | A person runs the companion's installer (`vibemon.dev/install.sh`, `…/install.ps1`). It installs the pinned release binary, then drives `attempt init`, `attempt sync connect --pair`, `attempt hook install`, `attempt daemon install`, `attempt sync now`, `attempt hook install --remove-legacy vibemon`, `attempt doctor` — in that order, which is its safety | [`migration/vibemon-install.sh`](migration/vibemon-install.sh), [`migration/vibemon-install.ps1`](migration/vibemon-install.ps1); `tests/installers/` |
| 2 | **Pairing** | The companion's backend mints a one-time token with the admin token; the installer checks and exchanges it for a device key bound to the local database's device id | [server-api.md](server-api.md) *Pairing*; [RFC 0006](rfcs/0006-privacy-and-sync.md) §10 |
| 3 | **Upload** | The daemon (`attempt sync`) sends events and, by profile, inferences and content to the sync server | [server-api.md](server-api.md) *Write side*; [RFC 0006](rfcs/0006-privacy-and-sync.md) |
| 4 | **Outbound webhook** | The sync server POSTs every accepted event to the companion, HMAC-signed, at least once, by per-tenant cursor | [server-api.md](server-api.md) *Outbound webhook* |
| 5 | **Operator read** | The companion's backend reads a tenant with the admin token plus `X-AttemptDB-Tenant`: status, live, sessions, timeline, work, attention, state, events, query | [server-api.md](server-api.md) *The operator's read*, *Read side* |
| 6 | **Event and inference shape** | Everything above carries Event v1 and Inference v1 | [`spec/`](../spec/README.md); [RFC 0001](rfcs/0001-canonical-event-model.md), [RFC 0003](rfcs/0003-fact-inference-bitemporal-model.md) |
| 7 | **Release policy** | Installed clients read `update.json` (from `RELEASE.toml`) daily; `required_below` forces an update (`min_sync_version` names the sync protocol version; published, not enforced yet). The companion's installers pin one version | [releasing.md](releasing.md); `RELEASE.toml` |
| 8 | **Legacy migration** | The old VibeMon client (`~/.vibemon/notify.sh`) posts to `POST /v1/vibemon/hook`; `attempt import vibemon-export` backfills its history; `--remove-legacy vibemon` retires it | [migration/vibemon-hooks.md](migration/vibemon-hooks.md); [server-api.md](server-api.md) |

## Before changing one of these

- **Surfaces 2–6** are HTTP and data contracts. Change the authoritative
  document in the same commit as the code, and keep its tests green
  (`crates/attemptdb-server/tests/`; `attempt conformance` against the
  schemas in `spec/`).
- **Surface 1** has two copies: the tested one here under `docs/migration/`,
  and the one the companion's web serves. The web's copy is updated by
  hand, and its pinned `ATTEMPTDB_VERSION` moves only after that release's
  assets exist.
- **The binary installer runs `attempt setup` since 0.2.14.** The companion's
  installers call it with `ATTEMPTDB_NO_SETUP=1` so their own order (pair
  first, then hooks and daemon) holds; `tests/installers` checks it. A web
  copy that bumps its pin to 0.2.14 or later needs the same line.
- **Surface 7** reaches every installed client within a day. Treat
  `required_below` as a kill switch, not a nudge.

## Loose ends, left for the companion's own planning

- The companion's installer orders init, pairing, hooks and daemon itself,
  where the local installers call `attempt setup` once. Both call the same
  binary, so no wiring logic is duplicated — but the *order* lives in two
  places.
- The companion reads through the operator read (surface 5) and receives
  the webhook (surface 4). Which of the two a given feature should use is
  the companion's decision; AttemptDB serves both.
