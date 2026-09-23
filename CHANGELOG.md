# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow SemVer.

## [Unreleased]

### Security
- Local per-account caches are SQLCipher databases (AES-256); existing plaintext caches are converted on first start.
- Session tokens are encrypted under per-account keys derived with HKDF from the master key; old blobs are re-wrapped on first use.
- Master-key rotation with `BRIDGE_MASTER_KEY_PREVIOUS`.
- Nightly database dumps can be encrypted to an `age` public key (`BACKUP_AGE_RECIPIENT`).
- Sign-in requests and code attempts are limited per Telegram user and hour.
- The bridge container runs with a read-only root filesystem, no capabilities and `no-new-privileges`.
- Dependency audit workflow (RustSec) and dependency review on pull requests.

### Changed
- Moved to Tzibbur's official API and stopped the polling that loaded the server:
  - Delivery is push-only over the WebSocket. The 60-second sweep that paged every group's history and sent a REST ack per group, per account, is gone, as is the REST catch-up on every reconnect. A connected account now makes no periodic requests; `GET /v1/pending` is used only while the socket is down (at most every 5 minutes).
  - Socket batches are acked on the socket with their last seq, as the protocol requires; REST acks are only used for REST-pulled batches. Reading uses the new `read` frame / `POST /v1/groups/{id}/read` instead of an ack.
  - Group events are parsed in the official shape (fields at the top level, `settings` nested), so renames, joins, leaves and role changes reach Telegram again.
  - Sends understand `{message, duplicate}` and in-chat command replies (`{command}`); a `#help` or `#add` typed in a topic now shows Tzibbur's reply instead of retrying forever.
  - The socket stops on close code 4001 or a 403 `device_blocked`, waits at least a minute after 4029 (too many connections), and honours `Retry-After` on a 429 handshake.
  - Official error codes (`posting_not_allowed`, `admin_required`, `device_blocked`, …) and status codes; defaults now match the server (1000-character messages, 100 members).
  - The client identifies itself honestly (`tzibbur-api-rs/…` User-Agent, device model `UnTzibburBot (Telegram bridge)`) instead of imitating the Android app.
  - New client calls: `GET /v1/capabilities`, `POST /v1/sessions/logout`, `DELETE /v1/me/devices/{id}`.
- CI runs a single cached job and skips the toolchain on documentation-only changes; deploys skip documentation-only pushes.

## [v0.1.0] - 2026-09-08

First public release.

- Telegram client for Tzibbur: every group becomes a topic in the user's chat with the bot; messages both ways.
- Sign-in by phone and SMS code in chat, or through a Mini App.
- Group management from Telegram: create, rename, permissions, members (add by phone or shared contact, promote, demote, remove), leave, delete.
- `/find` over a group's recent messages, live from Tzibbur.
- Several Tzibbur accounts per Telegram user, with an active-account switch.
- English, Hebrew and Yiddish for the main flows.
- Privacy by construction: message text is erased once delivered; only ids, mappings and an encrypted session are stored.
- Outage-safe delivery: per-group catch-up, delivery acknowledgements, re-forwarding after downtime, daily database backups.
- `tzibbur-api`: a Rust client for the Tzibbur REST and WebSocket API, verified against the live service, with a documented protocol reference.
- Deploys from GitHub Actions over Tailscale SSH; AGPL-3.0-or-later.
