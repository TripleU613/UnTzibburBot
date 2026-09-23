# Development

Everything that is not needed to simply use or run the bot.

## Layout

```
crates/
  tzibbur-api/   client library for the Tzibbur service: REST, WebSocket, SQLite cache,
                 sync engine, outbox (README inside)
  bridge/        the Telegram bot: teloxide handlers, per-account runtimes, Directus store
assets/          logo, avatar (for @BotFather), banner, README screens
docs/            How the client uses the official Tzibbur API, old decompiled app notes
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

Tzibbur publishes an official API contract: the integration guide at
<https://api.tzibbur.me/integration> and the OpenAPI spec at
<https://api.tzibbur.me/docs/openapi.json>. Those are authoritative; [`docs/tzibbur-api.md`](docs/tzibbur-api.md)
only records how this client uses them. (The old decompiled notes in
[`docs/android-app-notes.md`](docs/android-app-notes.md) are historical.) The points that bite:

- **Be gentle with the server.** Delivery is push: one WebSocket per account, which
  pushes the backlog after `hello` and new messages as they arrive. The engine never
  polls while the socket is up. Only when the socket is down does it fall back to
  `GET /v1/pending`, at most every 5 minutes.
- `ack` is a **delivery** acknowledgement: ack each socket batch *on the socket* with its
  last seq (the server holds the next batch for that group until then); REST acks are
  only for batches pulled over REST. `read` (frame or `POST /v1/groups/{id}/read`) is the
  separate per-user read position.
- Group event frames carry their fields at the top level (`groupId`, `userId`, …) and are
  never replayed, so the group list is re-fetched once per reconnect.
- `#add`, `#help` and the other in-chat commands are executed by the server and answered
  with `200 {"command": …}`; nothing is stored.
- New groups need `limits.minMembersToPost` members (3) before anyone can post; the
  server answers `409 group_too_small`.
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

## Mini App over Tailscale Funnel

The login Mini App needs a public HTTPS URL. On a Tailscale host this costs one command and
touches nothing else on the machine:

```sh
tailscale funnel --bg --set-path /tzibbur http://127.0.0.1:8081   # bridge port from docker-compose.yml
```

Then set `BRIDGE_PUBLIC_URL=https://<host>.<tailnet>.ts.net/tzibbur` in the host `.env`. The
bridge serves the app under both `/app` and `<prefix>/app`, so it works whether the proxy strips
the prefix or not. The tailnet policy needs `nodeAttrs: [{target: [<host tag>], attr: [funnel]}]`.
Long polling stays on; webhooks are optional (`BRIDGE_USE_WEBHOOK=true`).

## Durability

Tzibbur is the source of truth; nothing the bridge stores is needed to recover messages.

| Component | Holds | If down | If lost |
|---|---|---|---|
| Postgres/Directus | users, accounts, mappings | forwarding pauses, a 45-second flush catches up | users reconnect; daily `pg_dump` in the `backups` volume (14 days) |
| Bridge `/data` | sync cache, text until delivered | nothing | rebuilt from Tzibbur on connect |
| Telegram | the topics | sends retried by the flush | a deleted topic is recreated |

## Security model

Everything at rest is encrypted with keys derived from one master key (`crates/bridge/src/crypto.rs`):

| Data | Where | Protection |
|---|---|---|
| Session token per account | Directus `accounts.encrypted_session` | AES-256-GCM, key = HKDF(master, "session", tzibbur user id); blob prefixed `v2:` |
| Local cache per account (messages until delivered, members, outbox) | `/data/accounts/<id>.db` | SQLCipher (AES-256), key = HKDF(master, "cache", tzibbur user id) |
| Database dumps | `backups` volume | gzip, optionally `age`-encrypted to `BACKUP_AGE_RECIPIENT` |

Rotation: set `BRIDGE_MASTER_KEY_PREVIOUS` to the old key and a fresh `BRIDGE_MASTER_KEY`. On the next
start every running account's session is decrypted with whichever key works and re-encrypted under the
current one; caches are re-keyed in place (`PRAGMA rekey`). Legacy blobs from before v0.2 (raw AES-GCM
under the master key, no prefix) are upgraded the same way. Plaintext caches from before v0.2 are
converted with `sqlcipher_export` on first open.

Sign-in abuse: `App::allow_auth` allows 5 code requests and 15 code attempts per Telegram user per hour,
in the bot flow and in the Mini App alike.

The bridge container runs read-only with all capabilities dropped; it only writes to `/data` and `/tmp`.

What this does not give you: end-to-end encryption. Tzibbur and Telegram both see message text, so
the bridge has to as well, for the moment it forwards a message. See `SECURITY.md`.

## License

AGPL-3.0-or-later. Anyone may use, study, change and redistribute the code, including running it as a service, provided the complete corresponding source of their version is made available to its users under the same license. A closed or proprietary fork is not permitted.

## Conventions

- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` must pass (CI enforces).
- Bot text: short sentences, no emojis, no examples that could be a real person's data.
- Never log message text or tokens.

## Bot component details

See [`crates/bridge/README.md`](crates/bridge/README.md) for commands, configuration and the privacy model.
