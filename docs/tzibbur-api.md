# How this client uses the Tzibbur API

Tzibbur publishes an official contract for client developers. It is authoritative:

- Live reference: <https://api.tzibbur.me/docs>
- OpenAPI spec: <https://api.tzibbur.me/docs/openapi.json>

This page does not repeat that contract. It records how `crates/tzibbur-api` and the
Telegram bridge follow it, above all how they keep request volume low. The notes from
the decompiled Android app ([android-app-notes.md](android-app-notes.md)) are
historical; where they disagree with the official API, the official API wins.

- REST: `https://api.tzibbur.me/v1` · WebSocket: `wss://api.tzibbur.me/v1/ws` (protocol 1)
- Auth: `Authorization: Bearer <token>` on every call except `/v1/auth/start` and `/v1/auth/verify`
- Errors: RFC 9457 problem details; the code is the last segment of `type`

## Request budget

Each connected account is one Tzibbur device holding one WebSocket. Once connected:

| When | Requests |
|---|---|
| Connect / reconnect | `GET /v1/groups` (all pages), once. Group events are not replayed, so this is required. |
| Backlog and new messages | None. The server pushes them on the socket after `hello`. |
| Each `messages` frame | One `ack` frame on the socket (no HTTP). |
| A member joins a group | `GET /v1/groups/{id}/members`, to learn the new member's name. |
| A message sent from Telegram | `POST /v1/groups/{id}/messages`, retried with the same `clientMessageId` on transient errors and `Retry-After` on 429. |
| Socket down | `GET /v1/pending` + REST acks, at most every 5 minutes, until the socket is back. |
| Heartbeat | A `ping` frame after `hello.limits.heartbeatSeconds` (30 s) of outbound silence; server pings are answered. |

Nothing else runs on a timer. In particular, there is no per-group history paging, no
REST ack sweep, and no REST catch-up while the socket is up. The end-to-end test
(`crates/tzibbur-api/tests/e2e_mock_server.rs`) asserts that a connected session makes
no `GET /v1/pending`, no history request and no REST ack.

Reconnects use jittered exponential backoff (2 s doubling to 5 min, reset on `hello`).
The socket does not reconnect after close code `4001` (session revoked), a `401` or a
`403 device_blocked` at the handshake. It waits at least 60 s after `4029` (too many
connections) and honours `Retry-After` on a `429` handshake.

## Delivery

- The server keeps one un-acked batch in flight per group. The engine stores each
  `messages` frame, then acks its last seq **on the socket**; that releases the next batch.
  If the event loop lags and drops frames, it reconnects so the server redelivers them.
  It never REST-acks a socket batch.
- REST acks are only sent for batches pulled from `GET /v1/pending`, which is polled
  again while any group reports `hasMore`.
- `ack` (per device, delivery) and `read` (per user, the badge) are different. The
  engine acks automatically; `SyncEngine::mark_read` sends a `read` frame (or
  `POST /v1/groups/{id}/read` when offline).
- Messages are deduplicated by id and ordered by `seq` only. Gaps in `seq` are normal.

## Frames and replies the client relies on

- `group` frames carry their fields at the top level: `member-added {groupId, userId,
  role, joinedSeq, actorId}`, `member-removed {groupId, userId, actorId, reason}`,
  `role-changed {groupId, userId, role, actorId}`, `group-updated {groupId, name,
  settings, actorId}`, `group-deleted {groupId, actorId}`. A nested `payload` object is
  also accepted.
- `read {groupId, readSeq}` from the server updates the local read bookmark.
- `POST /v1/groups/{id}/messages` answers `201 {message, duplicate: false}`, or
  `200 {message, duplicate: true}` for a retry, or `200 {command: {name, ok, code,
  params, text}}` when the body was an in-chat command (`#add`, `#exit`, `#name`,
  `#mute`, `#unmute`, `#help`, …). A command is not stored; the bridge shows its
  `text` in the topic.
- Policy refusals are specific 403 codes with `errors.reason`: `posting_not_allowed`,
  `adding_members_not_allowed`, `admin_required`, `leave_not_allowed`. `device_blocked`
  is terminal: the runtime stops, like on a 401.

## Identity

The bridge enrolls with `platform: "android"` and `deviceModel: "UnTzibburBot (Telegram
bridge)"`; set `TZIBBUR_PLATFORM` / `TZIBBUR_DEVICE_MODEL` to change them. `web` is not
used because web sessions expire every 15 minutes and require a captcha. Requests carry
a `tzibbur-api-rs/<version>` User-Agent; the bridge's runtime clients use
`tzibbur-telegram-bridge/<version>`.

## Not used

- Webhooks: the official API has none. The WebSocket is the push channel.
- Push notifications (`/v1/sessions/current/push-subscription`): meant for handsets
  (FCM / Web Push); a server holding a socket does not need them.
- Email / Google / device-code sign-in, access applications, device activation:
  available in the API, not wired into the bot.
