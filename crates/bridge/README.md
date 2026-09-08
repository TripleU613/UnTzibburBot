# tzibbur-telegram-bridge

One public Telegram bot. Each Telegram user connects their own Tzibbur account.
Every Tzibbur group becomes a **topic in the user's private chat with the bot**
(Bot API 9.4+, requires *Threaded Mode* in @BotFather). Messages flow both ways.

```
Tzibbur WS/REST ─▶ tzibbur-api SyncEngine ─▶ AccountRuntime ─▶ Telegram topic
Telegram topic  ─▶ teloxide handlers      ─▶ AccountRuntime ─▶ outbox ─▶ Tzibbur
                              Directus: users · accounts · conversations · messages
```

## Features

- `/connect` — phone + SMS code in chat (the code message is deleted immediately and never stored), or the **≡ menu button** opens a Mini App login page when `BRIDGE_PUBLIC_URL` is set (initData is HMAC-verified).
- Initial import: every group → topic, last N messages as history; new groups create topics automatically; renames rename topics; deletion/removal closes the topic.
- Reply in a topic to post to the group; delivery is confirmed with a 👍 reaction, failures get a 👎 and an explanation. Text only (Tzibbur is text-only).
- `/newgroup` (name → category → members), `/add`, `/members`, `/leave`, `/mute`, `/name`, `/chats`, `/status`, `/sync`, `/settings`, `/legal`, `/disconnect` (keep or erase mappings), `/donate` (Telegram Stars).
- Session expiry (401) → account marked `reauth_required`, user gets a **Reconnect** button; topics are reused after reconnecting.
- Fallback when topics are unavailable: messages arrive tagged with the group name; replying to one routes the answer to that group.
- Privacy-minimal storage: Directus holds ids, mappings and the AES-256-GCM–encrypted session only. Message bodies live in a per-account SQLite cache under `BRIDGE_DATA_DIR` (needed for sync/dedup) and in Tzibbur.

## Privacy & threat model

The bridge is public and self-service: anyone can `/connect` their own Tzibbur
account. It is designed so the **operator cannot read users' messages**:

| Data | Where | Operator access |
|---|---|---|
| Message text | In memory only while relaying. Erased from the per-account SQLite cache the moment a message is delivered to Telegram or confirmed by Tzibbur (`redact_messages` / `redact_confirmed_outbox`). Never written to Directus or logs. | None at rest. |
| Message ids, seqs, sender ids | SQLite cache + Directus `bridge_messages` | Yes (needed for dedup/replies) |
| Group list, names, topic mapping | Directus `bridge_conversations` | Yes (metadata) |
| Telegram id, Tzibbur user id, phone | Directus `bridge_users` / `bridge_accounts` | Yes |
| Member names/phones of a user's groups | SQLite cache (for sender labels) | Yes |
| Tzibbur session token | Directus, AES-256-GCM with `BRIDGE_MASTER_KEY` (kept only in the bridge container env) | **Yes, unavoidably** — an always-on relay must hold the credential. |
| SMS codes | Nowhere (the OTP message is deleted from Telegram immediately) | None |
| Telegram chat history | Telegram never exposes it to bots | None |

What no bridge can promise: the process sees plaintext in transit and holds the
session, so a malicious operator could modify the code to capture either. Users
can see the same statement in the bot via `/privacy`; `/disconnect` erases the
session, and "Disconnect & erase" removes every mapping.

## Run

```sh
cp .env.example .env      # fill in TELOXIDE_TOKEN, DIRECTUS_*, BRIDGE_MASTER_KEY
docker compose up -d --build
docker compose logs -f bridge
```

On first start the bridge creates the Directus collections `bridge_users`,
`bridge_accounts`, `bridge_conversations`, `bridge_messages` (prefix configurable).
Directus UI: http://localhost:8055.

Without Docker: `cargo run -p tzibbur-telegram-bridge` with the same variables exported.

## Deploying (how the public bot runs)

`.github/workflows/deploy.yml` builds the image on GitHub, pushes it to GHCR, joins the
tailnet with a `tag:ci` auth key (`TS_AUTHKEY`, the only GitHub secret) and runs
`docker compose pull && up -d` on the host over Tailscale SSH. The host keeps its own
`.env` (mode 600) and data volumes; the workflow never reads or writes secrets.
`docker-compose.prod.yml` swaps the local build for the published image.

## Configuration

| Variable | Meaning |
|---|---|
| `TELOXIDE_TOKEN` | Bot token |
| `DIRECTUS_URL`, `DIRECTUS_TOKEN` | Directus base URL and a static token with admin rights |
| `BRIDGE_MASTER_KEY` | base64 32 bytes; encrypts Tzibbur sessions at rest |
| `BRIDGE_PUBLIC_URL` | optional public HTTPS URL → webhook mode at `/telegram/webhook`, Mini App at `/app` |
| `BRIDGE_LISTEN` | bind address (default `0.0.0.0:8080`; `/health`) |
| `BRIDGE_DATA_DIR` | per-account SQLite caches |
| `BRIDGE_HISTORY_IMPORT` | messages imported per group when its topic is created (default 20) |
| `BRIDGE_COLLECTION_PREFIX` | Directus collection prefix (default `bridge_`) |
| `TZIBBUR_BASE_URL` | default `https://api.tzibbur.me` |

## Layout

```
src/main.rs        startup: Directus bootstrap, Telegram profile, runtimes, dispatcher, HTTP
src/config.rs      env
src/directus.rs    REST client + schema bootstrap
src/store.rs       users / accounts / conversations / messages on Directus
src/crypto.rs      session encryption
src/app.rs         connect / disconnect glue
src/bridge/        AccountRuntime (Tzibbur ⇄ topics), formatting
src/telegram/      commands, dialogue states, handlers, callbacks, Stars
src/miniapp.rs     /health, /app (Mini App login), initData verification
```
