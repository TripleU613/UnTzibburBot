# Development

Everything that is not needed to simply use or run the bot.

## Layout

```
crates/
  tzibbur-api/   client library for the Tzibbur service: REST, WebSocket, SQLite cache,
                 sync engine, outbox (README inside)
  bridge/        the Telegram bot: teloxide handlers, per-account runtimes, Directus store
assets/          logo, banner, avatar
.github/         CI (fmt, clippy -D warnings, tests, docker build) and Deploy
```

## How it works

```
Tzibbur WS/REST ──▶ SyncEngine ──▶ AccountRuntime ──▶ Telegram topic
Telegram topic  ──▶ handlers   ──▶ AccountRuntime ──▶ outbox ──▶ Tzibbur
                    Directus: users · accounts · conversations · messages (ids only)
```

- One `AccountRuntime` per connected account. It owns the Tzibbur socket, a per-account
  SQLite cache under `BRIDGE_DATA_DIR`, and forwards events into the user's topics.
- Directus (Postgres) holds users, accounts (session encrypted with `BRIDGE_MASTER_KEY`),
  group-to-topic mappings and message-id mappings. Collections are created on startup.
- The main thread of the bot chat is the command center; every group is a topic.
- Message text is redacted from the cache as soon as it is delivered or confirmed.

## Tzibbur protocol notes

The client was reverse-engineered from the Android app (`tzibbur-re.md`) and then verified
against the live server; the live behaviour wins. The full list is in
[`crates/tzibbur-api/README.md`](crates/tzibbur-api/README.md). The ones that bite:

- Enums are lowercase (`admin`, `system`, `everyone`). Group settings are nested.
- `POST /v1/auth/start` and `/verify` require `platform` and `deviceModel`.
- New groups need `limits.minMembersToPost` members (3) before anyone can post; the
  server answers `409 group-too-small`.
- `ack` is a **delivery** acknowledgement. It advances the device's `deliveredSeq`; a device
  that never acks is re-sent everything on each connect. The engine acks every stored batch
  and sweeps with REST acks every minute.
- Message bodies arrive prefixed with the sender's name (`"Name: text"`).

## Running locally

```sh
cp .env.example .env
docker compose up -d db directus     # or point DIRECTUS_URL at any Directus 11
cargo run -p tzibbur-telegram-bridge
```

Set `RUST_LOG=info,tzibbur_api=debug` to see socket frames, batches and acks.

Never run two instances against the same bot token: Telegram long polling allows one consumer.

## Tests

```sh
cargo test --workspace                                   # unit + mock-server e2e
DIRECTUS_URL=... DIRECTUS_TOKEN=... cargo test -p tzibbur-telegram-bridge --test store_live
TZIBBUR_TOKEN=... cargo run -p tzibbur-api --example live_smoke   # every endpoint, throwaway group
TZIBBUR_TOKEN=... cargo run -p tzibbur-api --example probe        # read-only look at an account
```

## Deploying

`.github/workflows/deploy.yml` runs on every push to `main`:

1. Builds the image on GitHub and pushes it to GHCR.
2. Joins the tailnet with `TS_AUTHKEY` (a reusable, ephemeral, preauthorized key tagged `tag:ci`).
3. Over Tailscale SSH to `${{ vars.DEPLOY_HOST }}`, runs `docker compose pull && up -d`
   in `/opt/untzibburbot`.

The host keeps its own `.env` (mode 600) and volumes. The workflow never reads or writes
secrets, and nothing sensitive is in this repository. Required in GitHub: secret `TS_AUTHKEY`,
variable `DEPLOY_HOST`. The tailnet policy must allow `tag:ci` to reach the host on port 22
and to SSH as the deploy user.

## Durability

Tzibbur is the source of truth; nothing the bridge stores is needed to recover messages.

| Component | Holds | If down | If lost |
|---|---|---|---|
| Postgres/Directus | users, accounts, mappings | forwarding pauses, a 45-second flush catches up | users reconnect; daily `pg_dump` in the `backups` volume (14 days) |
| Bridge `/data` | sync cache, text until delivered | nothing | rebuilt from Tzibbur on connect |
| Telegram | the topics | sends retried by the flush | a deleted topic is recreated |

## Conventions

- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` must pass (CI enforces).
- Bot text: short sentences, no emojis, no examples that could be a real person's data.
- Never log message text or tokens.

## Bot component details

See [`crates/bridge/README.md`](crates/bridge/README.md) for commands, configuration and the privacy model.
