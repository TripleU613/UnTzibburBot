# Tzibbur API reference

The HTTP and WebSocket API of the Tzibbur service, as implemented by `crates/tzibbur-api`.
Everything here was verified against the live server in September 2026 unless marked
*unverified*. The original decompiled notes are in [android-app-notes.md](android-app-notes.md);
where the two disagree, this document is right.

- Base URL: `https://api.tzibbur.me`
- WebSocket: `wss://api.tzibbur.me/v1/ws`
- Auth: `Authorization: Bearer <token>` on every call except `/v1/auth/*`
- Content type: JSON. Timestamps are RFC 3339 strings. Ids are UUIDs.
- Enums are lowercase (`admin`, `member`, `system`, `everyone`, `admins`).

## Errors

RFC 7807 `application/problem+json`. The `type` slug selects the error; slugs use underscores.

```json
{
  "type": "urn:tzibbur:error:validation_failed",
  "title": "validation_failed",
  "status": 400,
  "detail": "Request validation failed",
  "requestId": "01a0…",
  "errors": [{ "path": "/phones", "message": "Too small: expected array to have >=1 items" }]
}
```

| Slug | HTTP | Meaning |
|---|---|---|
| `validation_failed` | 400 | Body or query invalid. `errors` is a list of `{path, message}` or an object `{field: reason}` (e.g. `{"phone": "unparseable"}`). |
| `invalid_display_name`, `reserved_display_name` | 400 | Display name rejected. |
| `invalid_group_name`, `invalid_category`, `invalid_message`, `contacts_batch_too_large` | 400 | As named. |
| `unauthorized` | 401 | Missing or revoked token. Clients should discard the session. |
| `invalid_code` | 401 | Wrong OTP. |
| `forbidden` | 403 | Not allowed (e.g. non-admin changing group settings). |
| `not_found` | 404 | Unknown route or resource. |
| `client_message_id_reused` | 409 | `clientMessageId` already used. |
| `group_too_small` | 409 | Group has fewer than `limits.minMembersToPost` members. |
| `group_full`, `last_admin`, `sms_delivery_failed` | 422 | As named. |
| `rate_limited` | 429 | Honour `Retry-After` / `retryAfterSeconds`. |
| `not_implemented` | 501 | |

## Authentication

### `POST /v1/auth/start`

Sends an SMS code. **`platform` and `deviceModel` are required** (not in the app notes).

```json
{ "phone": "+15550100123", "displayName": "Name", "region": "US",
  "platform": "android", "deviceModel": "Pixel 7" }
```

`platform` ∈ `kosher | android | ios | web`. `displayName` is needed on first registration.

Response: `{ "challengeId": "…", "resendAfterSeconds": 30 }` (an expiry field is *not* sent).

### `POST /v1/auth/verify`

```json
{ "challengeId": "…", "code": "123456", "phone": "+15550100123",
  "displayName": "Name", "region": "US", "platform": "android", "deviceModel": "Pixel 7" }
```

Response:

```json
{ "user": { "id": "…", "displayName": "Name", "phoneE164": "+1555…", "kind": "person",
            "createdAt": "2026-…Z", "email": null, "googleLinked": false },
  "device": { "id": "…", "platform": "android", "deviceModel": "Pixel 7",
              "registeredAt": "…Z", "lastSeenAt": "…Z", "userId": "…" },
  "token": "…" }
```

Each verify registers a device; a device holds its own delivery state (see *Delivery*).

## Profile

| Method | Path | Notes |
|---|---|---|
| `GET` | `/v1/me` | User object as above. |
| `PATCH` | `/v1/me` | `{ "displayName": "…" }` (required, max 64 code points). Returns the user. |
| `GET` | `/v1/me/devices` | `{ "items": [Device…] }` |

## Contacts

`POST /v1/contacts/check` with `{ "phones": ["+1…"], "region": "US" }` (1–100 numbers).
Response `{ "registered": ["+1…"] }` — plain E.164 strings. Numbers the server cannot parse
fail the whole request with `validation_failed` `{"phone": "unparseable"}`.

## Legal

`GET /v1/legal/{privacy|terms}` → `{ "document": { "key": "terms", "text": "# …", "checksum": "sha256hex" } }`.
Requires a token.

## Groups

Group object:

```json
{ "id": "…", "name": "Family", "category": "family", "kind": "standard",
  "createdBy": "…", "createdAt": "…Z", "role": "admin", "memberCount": 5, "muted": false,
  "readSeq": 0, "unreadCount": 3,
  "settings": { "whoCanPost": "everyone", "whoCanAddMembers": "admins" },
  "limits": { "memberCap": 100, "messageMaxLength": 1000, "minMembersToPost": 3 } }
```

`kind` is `standard` or `system` (the read-only "Tzibbur System" thread every user has).

| Method | Path | Body / query | Notes |
|---|---|---|---|
| `GET` | `/v1/groups` | `?cursor&limit` | `{ "items": [...], "nextCursor": null }` |
| `GET` | `/v1/groups/categories` | | `{ "categories": ["family","neighborhood","shul","school","other"] }` |
| `POST` | `/v1/groups` | `{ "name", "category" }` | Only these two are read; settings default to `everyone`. Creator is admin. |
| `GET` | `/v1/groups/{id}` | | 404 when gone. |
| `PATCH` | `/v1/groups/{id}` | `{ "name"?, "settings"?: { "whoCanPost"?, "whoCanAddMembers"? } }` | At least one of name/settings. Admins only. |
| `DELETE` | `/v1/groups/{id}` | | Admins only. A group is also deleted when its last member leaves. |
| `POST` | `/v1/groups/{id}/leave` | | |

## Members

| Method | Path | Body | Notes |
|---|---|---|---|
| `GET` | `/v1/groups/{id}/members` | `?cursor&limit` | `{ "items": [{ "userId","displayName","phoneE164","role","kind","joinedAt" }], "nextCursor": null }` |
| `POST` | `/v1/groups/{id}/members` | `{ "phones": ["+1…"], "region"? }` (1–100) | `{ "added": [...], "notFound": ["+1…"], "alreadyMember": [...] }` |
| `PATCH` | `/v1/groups/{id}/members/{userId}` | `{ "role": "admin" \| "member" }` | Demoting the only admin → `last_admin`. |
| `DELETE` | `/v1/groups/{id}/members/{userId}` | | Removing yourself is rejected; use leave. |

## Messages

Message object:

```json
{ "id": "…", "groupId": "…", "seq": 12, "senderId": "…", "body": "Name: text",
  "clientMessageId": "…", "createdAt": "…Z" }
```

`seq` is a per-group sequence starting at 1. **`body` is prefixed by the server with the
sender's display name and a colon.** Messages cannot be edited or deleted.

| Method | Path | Body / query | Notes |
|---|---|---|---|
| `GET` | `/v1/groups/{id}/messages` | `?afterSeq&beforeSeq&limit` (limit ≥ 1) | `{ "items": [...], "nextAfterSeq": null, "nextBeforeSeq": null }`. `afterSeq` appears to be inclusive; deduplicate by `id`. |
| `POST` | `/v1/groups/{id}/messages` | `{ "clientMessageId": "<uuid>", "body": "…" }` | Body ≤ `limits.messageMaxLength` (1000). Needs `minMembersToPost` members, else `409 group_too_small`. The reply is not always a full message object; clients should verify by `clientMessageId` if in doubt. |
| `POST` | `/v1/groups/{id}/ack` | `{ "seq": 12 }` | **Delivery** acknowledgement, see below. |

## Delivery

Each device has a per-group `deliveredSeq`. `GET /v1/pending` returns, for every group, the
messages above that mark:

```json
{ "groups": [ { "groupId": "…", "deliveredSeq": 4, "hasMore": false, "messages": [ … ] } ] }
```

The `ack` (REST, or the WebSocket frame) advances `deliveredSeq` to the given `seq`, and a REST
ack may jump forward arbitrarily. It does **not** change `readSeq`/`unreadCount`, which no
known endpoint writes. A device that never acks is re-sent everything on each connect and, in
practice, receives no live pushes until it acknowledges what it was sent. Store first, then ack.

## WebSocket

Connect to `/v1/ws` with the bearer header. Frames are JSON text; the server also sends
protocol-level pings.

Server → client:

| `type` | Fields |
|---|---|
| `hello` | `protocolVersion: 1`, `userId`, `deviceId`, `limits: { heartbeatSeconds: 30, maxConnectionsPerDevice: 3, maxFrameBytes: 16384 }` |
| `messages` | `groupId`, `messages: [Message]`, `hasMore` — the pending backlog right after `hello`, then live pushes |
| `group` | `event`, `payload` — events `member-added {groupId, member}`, `member-removed {groupId, userId}`, `role-changed {groupId, userId, role}`, `group-updated {groupId, name?, whoCanPost?, whoCanAddMembers?}`, `group-deleted {groupId}` *(shapes from the app notes; not all observed)* |
| `pong` | reply to `ping` |
| `error` | `code`, `detail?` — a version/update code means the client must upgrade |

Client → server:

| `type` | Fields |
|---|---|
| `ping` | send after ~30 s of outbound silence |
| `ack` | `groupId`, `seq` — delivery acknowledgement |

Close code `4029` means too many connections for the device (limit 3).

## Limits observed

| What | Value |
|---|---|
| Message length | 1000 code points (`limits.messageMaxLength`) |
| Members per group | 100 (`limits.memberCap`) |
| Members before posting | 3 for standard groups, 0 for system (`limits.minMembersToPost`) |
| Phones per contacts/members call | 100 |
| Display name | 64 code points |
| Group name | 100 code points |
| Connections per device | 3 |

## Unverified

- Whether `POST /v1/groups` accepts a nested `settings` object at creation.
- Exact payloads of the `group` WebSocket events beyond what the app notes state.
- Whether any endpoint advances `readSeq`.
