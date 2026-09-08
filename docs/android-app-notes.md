# Tzibbur App — Reverse Engineering Reference

**Package:** `com.tzibbur.app` | **Version:** 0.1.0-debug | **DEX files:** 25  
**HTTP client:** Ktor | **Serialization:** kotlinx.serialization | **DI:** Dagger/Hilt  
**UI:** Jetpack Compose | **DB:** Room (SQLite) | **Async:** Kotlin Coroutines + Flow

---

## Table of Contents

1. [REST API](#rest-api)
2. [WebSocket Protocol](#websocket-protocol)
3. [Database Schema](#database-schema)
4. [Sync Engine](#sync-engine)
5. [Auth & Session](#auth--session)
6. [Feature Flows](#feature-flows)
7. [Error Handling](#error-handling)
8. [Domain Layer](#domain-layer)

---

## REST API

**Base URL:** `https://api.tzibbur.me`  
**Auth:** `Authorization: Bearer {token}` on all authenticated endpoints  
**Error format:** RFC 7807 Problem+JSON

### Auth

#### `POST /v1/auth/start`
Start phone auth — triggers SMS OTP delivery.

| Field | Type | Notes |
|---|---|---|
| `phone` | String | E.164 international format |
| `displayName` | String? | Required on first registration |
| `region` | String? | ISO 3166-1 alpha-2 |

**Response:** `EnrollmentChallenge { challengeId, expiresAtEpochMs, resendAfterSeconds }`

#### `POST /v1/auth/verify`
Verify OTP code → receive bearer token.

| Field | Type | Notes |
|---|---|---|
| `challengeId` | String | From /auth/start response |
| `code` | String | 6-digit OTP |
| `phone` | String | E.164 |
| `displayName` | String? | Set on first registration |
| `region` | String? | ISO 3166-1 alpha-2 |

**Response:** `Session { user { id, displayName, phoneE164 }, device { id, ... }, token }`  
Token is AES-GCM encrypted via Android Keystore and stored in DataStore.

---

### Profile

#### `GET /v1/me`
Returns `User { id, displayName, phoneE164 }`

#### `PATCH /v1/me`
Update display name (max 64 code-points).

| Field | Type |
|---|---|
| `displayName` | String |

#### `GET /v1/me/devices`
List registered devices. Used in Settings → Devices screen.

---

### Contacts

#### `POST /v1/contacts/check`
Check which phone numbers are registered on Tzibbur. Max 100 phones per call.

| Field | Type | Notes |
|---|---|---|
| `phones` | List\<String\> | E.164, max 100 |
| `region` | String? | ISO country code |

Requires `READ_CONTACTS` permission on device; gracefully degrades to empty list without it.

---

### Legal

#### `GET /v1/legal/{key}`
Fetch legal document. `key` values: `privacy`, `terms`.  
Returns markdown document + checksum. Cached in `LegalStore`; `lastSeenChecksum` tracks whether user has acknowledged current version.

---

### Groups

#### `POST /v1/groups`
Create group.

| Field | Type | Notes |
|---|---|---|
| `name` | String | Max 100 code-points |
| `category` | String | Must be valid category slug |
| `kind` | GroupKind | STANDARD \| SYSTEM |
| `whoCanPost` | String | Permission enum |
| `whoCanAddMembers` | String | Permission enum |

#### `GET /v1/groups`
List groups (paginated).

| Param | Type |
|---|---|
| `cursor` | String? |
| `limit` | Int? |

#### `GET /v1/groups/categories`
Get group category slugs. Cached in `AppPrefsStore` as `CategoriesEnvelope { categories: List<String>, fetchedAt: String }`.

#### `GET /v1/groups/{id}`
Get group by ID. On 404 → `markGroupDeleted` via `DropGroupOnNotFound` middleware.

#### `PATCH /v1/groups/{id}`
Update group settings.

| Field | Type |
|---|---|
| `name` | String? |
| `whoCanPost` | String? |
| `whoCanAddMembers` | String? |

#### `DELETE /v1/groups/{id}`
Delete group (admin only).

---

### Members

#### `GET /v1/groups/{id}/members`
List members (paginated).

| Param | Type |
|---|---|
| `cursor` | String? |
| `limit` | Int? |

#### `POST /v1/groups/{id}/members`
Add members by phone number. Max 100 per batch.

| Field | Type |
|---|---|
| `phones` | List\<String\> |
| `region` | String? |

**Response:** `AddMembersOutcome { added, notFound, alreadyMember }`

#### `DELETE /v1/groups/{id}/members/{userId}`
Remove a member.

#### `PATCH /v1/groups/{id}/members/{userId}`
Set member role.

| Field | Type | Notes |
|---|---|---|
| `role` | String | ADMIN \| MEMBER |

Cannot demote self if last admin → `LastAdmin` error.

#### `POST /v1/groups/{id}/leave`
Leave a group.

---

### Messages

#### `GET /v1/groups/{id}/messages`
Get messages, paginated by sequence number.

| Param | Type | Notes |
|---|---|---|
| `afterSeq` | Long? | Exclusive lower bound |
| `beforeSeq` | Long? | Exclusive upper bound |
| `limit` | Int? | |

#### `POST /v1/groups/{id}/messages`
Send message. Not called directly — goes through the outbox dispatch loop.

| Field | Type | Notes |
|---|---|---|
| `clientMessageId` | String | UUID, for deduplication / echo detection |
| `body` | String | Max 2000 code-points |

#### `POST /v1/groups/{id}/ack`
Acknowledge messages read up to a sequence number. Also callable via WebSocket `ack` frame. Updates `lastReadSeq` in DB.

| Field | Type |
|---|---|
| `seq` | Long |

---

### Pending Catch-up

#### `GET /v1/pending`
Get missed events since last connection. Called during `SyncEngine.restCatchUp()` on reconnect.

| Param | Type |
|---|---|
| `limit` | Int? |

---

## WebSocket Protocol

**URL:** `wss://api.tzibbur.me/v1/ws`  
**Auth:** `Authorization: Bearer {token}` header at handshake  
**Protocol version:** `1`  
**Event buffer:** `MutableSharedFlow(replay=0, extraBufferCapacity=256)`

### Connection Lifecycle

```
Idle → Connecting → Connected ⟲ BackingOff
                 ↘ UpdateRequired
```

- Socket close code **4029** = too many connections
- Server pings client after `PING_AFTER_OUTBOUND_SILENCE` of outbound silence
- On 401 → fires `SessionInvalidationListener.onSessionInvalidated()` → `SessionScopeManager.wipeSession()`

### Client → Server Frames

| Frame type | Fields | Notes |
|---|---|---|
| `ping` | — | Sent after outbound silence period |
| `ack` | `groupId, seq` | Mark messages as read |

### Server → Client Frames

| Frame type | Fields | Notes |
|---|---|---|
| `hello` | `version: Int` | Server capability handshake on connect |
| `pong` | — | Response to client ping |
| `messages` | `groupId, messages: List<MessageDto>` | New messages pushed in real-time |
| `group` | `event: String, payload: JsonObject` | Group lifecycle events (see below) |
| `error` | `code: String, detail: String?` | Protocol-level error (e.g. version mismatch) |

### Group Event Sub-types (`group` frame)

| `event` value | Payload fields | DB action |
|---|---|---|
| `member-added` | `groupId, member: MemberDto` | Upsert member, refresh if observed |
| `member-removed` | `groupId, userId` | `deleteMember` |
| `role-changed` | `groupId, userId, role` | `UPDATE members SET role` |
| `group-updated` | `groupId, name?, whoCanPost?, whoCanAddMembers?` | `applyGroupUpdated` |
| `group-deleted` | `groupId` | `markGroupDeleted` |

### Internal SocketEvent Types

| Type | Fields |
|---|---|
| `SocketEvent.Connected` | — |
| `SocketEvent.Disconnected` | `reason: DisconnectReason` |
| `SocketEvent.Messages` | `groupId, messages` |
| `SocketEvent.GroupEvent` | `kind, groupId, payload` |

### DisconnectReason

| Type | Fields |
|---|---|
| `DisconnectReason.Closed` | `code: Int?, message: String?` |
| `DisconnectReason.Error` | `cause: Throwable?` |
| `DisconnectReason.UpdateRequired` | — (triggers app update prompt) |

---

## Database Schema

Room database, SQLite. **5 tables.** Schema version **2**.  
**Migration 1→2:** `ALTER TABLE groups ADD COLUMN lastReadSeq INTEGER NOT NULL DEFAULT 0`

### `groups` table

```sql
CREATE TABLE `groups` (
  `id`                 TEXT    NOT NULL,
  `name`               TEXT    NOT NULL,
  `category`           TEXT    NOT NULL,
  `kind`               TEXT    NOT NULL,          -- STANDARD | SYSTEM | UNKNOWN
  `whoCanPost`         TEXT    NOT NULL,
  `whoCanAddMembers`   TEXT    NOT NULL,
  `createdBy`          TEXT,                       -- userId, nullable
  `createdAt`          INTEGER NOT NULL,           -- epoch ms
  `myRole`             TEXT    NOT NULL,
  `memberCount`        INTEGER NOT NULL,
  `muted`              INTEGER NOT NULL,           -- 0/1 boolean
  `lastActivityAt`     INTEGER,
  `lastMessagePreview` TEXT,
  `isDeleted`          INTEGER NOT NULL DEFAULT 0,
  `lastReadSeq`        INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(`id`)
)
```

**GroupDao operations:**

| Method | SQL pattern |
|---|---|
| `upsert()` | `INSERT OR IGNORE` |
| `get(id)` | `SELECT * WHERE id = ?` |
| `markRead(id, seq)` | `UPDATE SET lastReadSeq = MAX(lastReadSeq, ?)` |
| `markDeleted(id)` | `UPDATE SET isDeleted = 1 WHERE id = ?` |
| `markAllDeleted()` | `UPDATE SET isDeleted = 1` |
| `markDeletedExcept(ids)` | `UPDATE SET isDeleted = 1 WHERE id NOT IN (...)` |
| `setMuted(id, muted)` | `UPDATE SET muted = ?` |
| `purgeDeleted()` | `DELETE WHERE isDeleted = 1` |
| `observeGroupsWithUnread(selfId)` | Flow — JOIN with messages count `WHERE seq > lastReadSeq` |

---

### `messages` table

```sql
CREATE TABLE `messages` (
  `id`              TEXT    NOT NULL,
  `groupId`         TEXT    NOT NULL,
  `seq`             INTEGER NOT NULL,
  `senderId`        TEXT    NOT NULL,
  `body`            TEXT    NOT NULL,
  `clientMessageId` TEXT,                         -- nullable; set if this user sent it
  `createdAt`       INTEGER NOT NULL,
  PRIMARY KEY(`id`)
)
CREATE UNIQUE INDEX `index_messages_groupId_seq` ON `messages` (`groupId` ASC, `seq` DESC)
```

**MessageDao operations:**

| Method | SQL pattern |
|---|---|
| `insert()` | `INSERT OR IGNORE` |
| `observeThread(groupId, limit)` | `ORDER BY seq ASC` — Flow |
| `maxSeq(groupId)` | `SELECT MAX(seq)` |
| `minSeq(groupId)` | `SELECT MIN(seq)` |
| `latestMessage(groupId)` | `ORDER BY seq DESC LIMIT 1` |
| `countBetween(groupId, from, to)` | `COUNT WHERE seq BETWEEN ? AND ?` |
| `deleteForGroup(groupId)` | `DELETE WHERE groupId = ?` |

---

### `members` table

```sql
CREATE TABLE `members` (
  `groupId`     TEXT    NOT NULL,
  `userId`      TEXT    NOT NULL,
  `displayName` TEXT    NOT NULL,
  `phoneE164`   TEXT,
  `role`        TEXT    NOT NULL,                 -- ADMIN | MEMBER
  `joinedAt`    INTEGER NOT NULL,
  PRIMARY KEY(`groupId`, `userId`),
  FOREIGN KEY(`groupId`) REFERENCES `groups`(`id`) ON DELETE CASCADE
)
CREATE INDEX `index_members_groupId` ON `members` (`groupId`)
```

**MemberDao operations:**

| Method | SQL pattern |
|---|---|
| `upsert()` | `INSERT … ON CONFLICT UPDATE` |
| `observeMembers(groupId)` | `ORDER BY joinedAt ASC, userId ASC` — Flow |
| `delete(groupId, userId)` | `DELETE WHERE groupId = ? AND userId = ?` |
| `deleteAll(groupId)` | `DELETE WHERE groupId = ?` |
| `setRole(groupId, userId, role)` | `UPDATE SET role = ?` |

---

### `outbox` table

```sql
CREATE TABLE `outbox` (
  `clientMessageId`    TEXT    NOT NULL,           -- UUID, primary key
  `groupId`            TEXT    NOT NULL,
  `body`               TEXT    NOT NULL,
  `state`              TEXT    NOT NULL,           -- PENDING | IN_FLIGHT | CONFIRMED
  `errorCode`          TEXT,
  `attemptCount`       INTEGER NOT NULL,
  `nextAttemptAt`      INTEGER,                    -- epoch ms
  `lastAttemptAt`      INTEGER,
  `createdAt`          INTEGER NOT NULL,
  `confirmedMessageId` TEXT,                       -- set after server confirms
  `confirmedSeq`       INTEGER,
  PRIMARY KEY(`clientMessageId`)
)
CREATE INDEX `index_outbox_groupId` ON `outbox` (`groupId`)
CREATE INDEX `index_outbox_state`   ON `outbox` (`state`)
```

**OutboxState enum:** `PENDING` → `IN_FLIGHT` → `CONFIRMED`

**OutboxDao operations:**

| Method | SQL pattern |
|---|---|
| `insert()` | `INSERT OR IGNORE` |
| `pendingCount()` | `COUNT WHERE state IN ('PENDING', 'IN_FLIGHT')` |
| `unconfirmedIds(groupId)` | `SELECT clientMessageId WHERE state != 'CONFIRMED'` |
| `get(clientMessageId)` | Single row |
| `delete(clientMessageId)` | Delete by PK |
| `purgeOldConfirmed(before)` | `DELETE WHERE state='CONFIRMED' AND createdAt < ?` |
| `nextDispatchable(now, staleBefore)` | Picks PENDING or stale IN_FLIGHT |
| `markInFlight(id, at)` | Set state=IN_FLIGHT, lastAttemptAt |
| `markRejected(id, errorCode)` | Set state=PENDING, errorCode, reschedule |
| `reschedule(id, nextAt, attemptCount)` | Set nextAttemptAt, attemptCount++ |

---

### `local_command_replies` table

```sql
CREATE TABLE `local_command_replies` (
  `id`          INTEGER PRIMARY KEY AUTOINCREMENT,
  `groupId`     TEXT    NOT NULL,
  `commandName` TEXT    NOT NULL,
  `ok`          INTEGER NOT NULL,                  -- 0/1 boolean
  `code`        TEXT    NOT NULL,
  `paramsJson`  TEXT,
  `text`        TEXT    NOT NULL,
  `createdAt`   INTEGER NOT NULL
)
CREATE INDEX `index_local_command_replies_groupId` ON `local_command_replies` (`groupId`)
```

Stores replies to in-chat slash commands, shown as system bubbles in the thread. Cleared per-group on demand via `LocalCommandReplyDao`.

---

## Sync Engine

### SyncEngine State Machine

```
Idle ──start()──▶ Connecting ──hello──▶ Connected
                                            │
                              error(UpdateRequired)
                                            ▼
                                    UpdateRequired ──stop()──▶ Idle
                                                    (UpdateRequired preserved across stop/start)
```

### SyncEngine Methods

| Method | Behaviour |
|---|---|
| `start()` | Sets Connecting, launches coroutine (UNDISPATCHED), starts socket + outbox + calls `poke()` |
| `stop()` | Stops socket, cancels job, sets Idle (preserves UpdateRequired) |
| `refreshNow()` | If not Connected → `restCatchUp()`. Then `outbox.poke()` + `groupRepository.refreshGroups()` |
| `restCatchUp()` | Calls `GET /v1/pending`, applies missed messages to DB |
| `reconcileGroups()` | Calls `GET /v1/groups`, marks deleted any not in server list |

### SyncLifecycle

Ties `SyncController` to app foreground state and `SessionState`. Observes `ProcessLifecycleOwner` + `sessionRepository.sessionState` via `combine()`. When foregrounded AND `SignedIn` → `syncController.start()`. When backgrounded or `SignedOut` → `syncController.stop()`.

### OutboxDispatcher

**Constants:**

| Constant | Value |
|---|---|
| `BACKOFF_BASE` | 2 seconds |
| `BACKOFF_CAP` | 5 minutes |
| `FALLBACK_POLL` | 15 seconds |
| `IN_FLIGHT_STALE` | 2 minutes |

**Loop Steps:**

| Step | Meaning |
|---|---|
| `Step.Processed` | Message dispatched; loop again immediately |
| `Step.Idle` | Nothing pending; wait for signal on `Channel<Unit>` |
| `Step.WaitUntil(at)` | Next attempt scheduled; sleep until then |

**Retry / Backoff:**  
Formula: `min(BACKOFF_CAP, BACKOFF_BASE << cappedExponent)` with jitter from `Random`. IN_FLIGHT entries older than `IN_FLIGHT_STALE` (2 min) are treated as stale and re-dispatched. Between dispatches, if no pending count the loop waits on a `Channel<Unit>` signal; `poke()` sends a unit to wake it.

### BatchReconciler

Runs inside a Room transaction (`DeliveryTransactions.storeIncomingBatch`). Given a batch of `MessageEntity` items and current DB state (existingIds, existingSeqs, unconfirmedClientMessageIds), produces a `ReconcilePlan`:

| Field | Type | Meaning |
|---|---|---|
| `toInsert` | `List<MessageEntity>` | Net-new, non-duplicate messages to persist |
| `echoConfirmations` | `List<EchoConfirmation>` | Our own sent messages confirmed by server (clientMessageId → serverId + seq) |
| `newMessageIds` | `List<String>` | IDs of truly new (not echo) messages for unread count |
| `maxSeq` | `Long?` | Highest seq in batch, used to advance lastReadSeq bookmark |

Echo detection: if `clientMessageId` is in `unconfirmedClientMessageIds` → it's our own sent message bouncing back. Inserts the message but routes it as a confirmation rather than new incoming.

### MemberObservationRegistry

Ref-counted set of group IDs currently observed by the UI. `retain(groupId)` increments; `release(groupId)` decrements and removes at zero. SyncEngine checks `observedGroupIds()` to decide whether to eagerly refresh members on a `member-added` WS event (avoids fetching for off-screen groups).

### LocalStore Interface

Façade over all DAOs. Key methods:

```
observeThread(groupId, limit): Flow<List<MessageEntity>>
observeGroup(id): Flow<GroupEntity>
observeGroupsWithUnread(selfUserId): Flow<List<GroupWithUnread>>
observeMembers(groupId): Flow<List<MemberEntity>>
observePendingOutbox(groupId): Flow<List<OutboxEntity>>
observeCommandReplies(groupId): Flow<List<LocalCommandReplyEntity>>
insertMessages(msgs): suspend
confirmSent(clientMessageId, confirmedMsg): suspend   -- moves outbox to CONFIRMED
markGroupDeleted(id): suspend
reconcileGroups(groups): suspend                      -- upsert + markDeletedExcept
replaceMembers(groupId, members): suspend             -- deleteAll + bulk upsert
countBetween(groupId, from, to): suspend
nextDispatchable(now, staleBefore): suspend
markInFlight / markRejected / reschedule(…)
applyGroupUpdated(id, name, whoCanPost, whoCanAddMembers): suspend
applyCommandReply(groupId, entity): suspend
```

---

## Auth & Session

### Auth Flow

```
PhoneEntryViewModel.submit()
  → POST /v1/auth/start { phone, displayName?, region? }
  ← EnrollmentChallenge { challengeId, expiresAtEpochMs, resendAfterSeconds }
  → navigate to CodeEntryRoute(challengeId, expiresAtEpochMs, resendAfterSeconds,
                                phone, displayName?, region?)

CodeEntryViewModel.submit(code)
  → POST /v1/auth/verify { challengeId, code, phone, displayName?, region? }
  ← Session { user, device, token }
  → SessionStore.save(user, device, token)  -- encrypts token with AES-GCM Keystore
  → SyncLifecycle triggers SyncEngine.start()
```

### SessionStore

DataStore\<Preferences\> backed, encrypted with `KeystoreSecretCipher`. Exposes:

| Flow / method | Type |
|---|---|
| `session` | `Flow<Session>` — combines user + device |
| `token()` | `suspend → String?` — reads and decrypts bearer token |
| `save(user, device, token)` | `suspend` |
| `updateDisplayName(name)` | `suspend` |
| `clear()` | `suspend` |

DataStore key: `TOKEN_CIPHERTEXT` (Base64 of AES-GCM ciphertext).

### KeystoreSecretCipher

| Detail | Value |
|---|---|
| Algorithm | `AES/GCM/NoPadding` |
| Tag length | 128 bits |
| Key alias | `tzibbur_session` |
| Key provider | Android Keystore (`AndroidKeyStore`) |
| Wire format | `[ivLength: 1 byte][iv: ivLength bytes][ciphertext: rest]` |

### Session Model

```kotlin
data class Session(
    val user: User,    // id, displayName, phoneE164
    val device: Device // id, ...
)
```

### SessionState

| State | Meaning |
|---|---|
| `Loading` | DataStore read in progress; show splash |
| `SignedOut` | No token; show onboarding |
| `SignedIn(user, token)` | Active session; show main app |

### SessionScopeManager (Logout / Wipe)

Implements `SessionInvalidationListener`. On `wipeSession()`:
1. Acquires mutex (idempotent via `AtomicBoolean wipeInFlight`)
2. Calls `syncController.stop()`
3. Clears `LocalStore` (all DB tables)
4. Calls `SessionStore.clear()`

Also triggered automatically when any HTTP request returns 401.

### AppPrefsStore (non-sensitive prefs)

Plain DataStore (not encrypted).

| Key | Type | Notes |
|---|---|---|
| `CATEGORIES_JSON` | `String?` | JSON-encoded `CategoriesEnvelope { categories: List<String>, fetchedAt: String }` |
| `THEME_OVERRIDE` | `ThemeOverride` | SYSTEM \| LIGHT \| DARK |

### LegalStore

DataStore. Stores legal documents keyed by `LegalDocKey` (`privacy` / `terms`). Tracks `lastSeenChecksum` to prompt re-acceptance when content changes.

---

## Feature Flows

### Navigation Routes (Type-Safe)

| Route | Graph | Params |
|---|---|---|
| `OnboardingGraphRoute` | Onboarding nested nav | — |
| `WelcomeRoute` | Onboarding | — |
| `PhoneEntryRoute` | Onboarding | — |
| `CodeEntryRoute` | Onboarding | challengeId, expiresAtEpochMs, resendAfterSeconds, phone, displayName?, region? |
| `LegalRoute` (onboarding) | Onboarding | key (privacy\|terms) |
| `GroupListRoute` | Groups | — |
| `CreateGroupRoute` | Groups | — |
| `ChatRoute` | Chat | groupId |
| `GroupDetailRoute` | GroupDetail | groupId |
| `AddMembersRoute` | GroupDetail | groupId |
| `SettingsGraphRoute` | Settings | — |
| `SettingsHomeRoute` | Settings | — |
| `DevicesRoute` | Settings | — |
| `LegalRoute` (settings) | Settings | key |

### Onboarding Feature

**PhoneEntryViewModel:**
- `uiState: StateFlow<PhoneEntryUiState>` — fields: phone, displayName, nameError, rateLimitError, isSubmitting, rateLimitSecondsLeft
- `proceedToCode: Flow<CodeEntryRoute>` — one-shot Channel event on success
- `submit()` → `POST /v1/auth/start`; starts rate-limit countdown on 429
- Default rate-limit wait: **60 seconds**
- Validates phone looks like a phone number before sending

**CodeEntryViewModel:**

| Constant | Value |
|---|---|
| `CODE_LENGTH` | 6 |
| `DEFAULT_RESEND_WAIT` | 60 seconds |
| `SUGGEST_RESEND_AFTER_FAILURES` | 3 |

- Supports SMS OTP auto-read via `SmsUserConsentOtpAutoReader` (Android SMS User Consent API)
- OTP extracted from SMS text via `OtpMessageParserKt` regex
- Countdown timer from `resendAfterSeconds`; shows "resend" after 3 failures

### Chat Feature

**ChatUiState fields:**

| Field | Type | Notes |
|---|---|---|
| `phase` | `ChatPhase` | LOADING \| READY |
| `groupName` | `String` | |
| `memberCount` | `Int` | |
| `isSystemThread` | `Boolean` | GroupKind.SYSTEM |
| `muted` | `Boolean` | |
| `showMutedHint` | `Boolean` | Dismissible hint banner |
| `items` | `List<ChatListItem>` | Feed items |
| `loadingOlder` | `Boolean` | Pagination in progress |
| `composer` | `ComposerUiState` | Input area state |
| `notAllowedBanner` | `Boolean` | User lacks post permission |
| `genericErrorBanner` | `GenericErrorUi?` | |
| `unseenCount` | `Int` | Scroll-to-bottom badge count |
| `scrollToBottomTick` | `Long` | Monotonic, triggers scroll animation |

**ChatListItem types:**

| Type | Fields |
|---|---|
| `ChatListItem.DayMarker` | `key, epochDay` |
| `ChatListItem.Incoming` | message + sender label |
| `ChatListItem.Outgoing` | message + OutgoingState |
| `ChatListItem.System` | `SystemText` (localized system message) |

**ComposerUiState:**

| State | When |
|---|---|
| `Hidden` | GroupKind.SYSTEM — no text input ever |
| `Locked` | User lacks `whoCanPost` permission |
| `Enabled(draft, isValid, codePoints, isNearLimit, maxLength, commandChips)` | Normal posting; warns at 1800/2000 chars |

**SystemText:**

| Type | Fields |
|---|---|
| `SystemText.Localized` | `code: SystemCode, arg: String?` |
| `SystemText.Raw` | `text: String` |

### Groups Feature

**GroupListViewModel:** Combines groups Flow + syncState Flow + refreshing state + refreshError into `GroupListUiState`. Pull-to-refresh calls `SyncEngine.refreshNow()`.

### Group Detail Feature

**GroupDetailDialog types:**

| Dialog | When shown |
|---|---|
| `RemoveMember(member)` | Remove member confirmation |
| `Rename(name, error?)` | In-place rename sheet; errors: EMPTY, TOO_LONG |
| `Leave` | Confirm leave group |
| `DeleteFirst` | First step of two-step group delete |
| `DeleteSecond` | Second confirmation (destructive) |

### Settings Feature

Screens: SettingsHome, Devices (`GET /v1/me/devices`), LegalViewer (from LegalStore cache or fetch). ThemeOverride toggle (SYSTEM/LIGHT/DARK) persisted to AppPrefsStore.

---

## Error Handling

All errors implement `AppError` interface with `getRequestId(): String?`.  
HTTP errors mapped from RFC 7807 Problem+JSON via `ErrorMapperKt`: the type URI suffix after the last `:` selects the error subtype.

### ProblemDto (RFC 7807)

```json
{
  "type": "urn:tzibbur:error:invalid-display-name",
  "title": "...",
  "status": 400,
  "detail": "...",
  "requestId": "...",
  "errors": { ... }
}
```

### AppError Subtypes

| Class | Fields | HTTP / Trigger |
|---|---|---|
| `ValidationFailed` | `requestId?, errors: JsonElement?` | 400 — generic field errors |
| `InvalidDisplayName` | `maxLength: Int (default 64), requestId?` | 400 — display name validation |
| `ReservedDisplayName` | `requestId?` | 400 — name is reserved/system |
| `InvalidGroupName` | `maxLength: Int (default 100), requestId?` | 400 — group name too long |
| `InvalidCategory` | `requestId?` | 400 — unknown category slug |
| `InvalidMessage` | `requestId?` | 400 — body exceeds 2000 chars |
| `ContactsBatchTooLarge` | `requestId?` | 400 — phones[] > 100 |
| `ClientMessageIdReused` | `requestId?` | 409 — duplicate clientMessageId |
| `Unauthorized` | `requestId?` | 401 — no/invalid token → wipe session |
| `InvalidCode` | `requestId?` | 401 — wrong OTP |
| `Forbidden` | `requestId?` | 403 — lacks permission |
| `NotFound` | `requestId?` | 404 — resource missing → mark group deleted |
| `GroupFull` | `maxMembers: Int (default 200), requestId?` | 422 — member limit exceeded |
| `LastAdmin` | `requestId?` | 422 — cannot demote/remove last admin |
| `SmsDeliveryFailed` | `requestId?` | 422 — OTP SMS not delivered |
| `RateLimited` | `retryAfterSeconds: Int?, requestId?` | 429 — too many requests |
| `NotImplemented` | `requestId?` | 501 — feature not available |
| `Internal` | `requestId?` | 5xx — server error |
| `Network` | `cause: Throwable?` | No response / timeout |
| `Unknown` | `status: Int?, requestId?` | Any unmapped error |

### Validation Constants

| Constant | Value | Used by |
|---|---|---|
| `DEFAULT_MAX_DISPLAY_NAME` | 64 code-points | `ValidateDisplayNameUseCase` |
| `DEFAULT_MAX_GROUP_NAME` | 100 code-points | `ValidateGroupNameUseCase` |
| `DEFAULT_MAX_MEMBERS` | 200 | `GroupFull` error |
| `DEFAULT_MAX_MESSAGE` | 2000 code-points | `ValidateMessageBodyUseCase`; composer warns at 1800 |
| `DEFAULT_MAX_PHONES` | 100 | contacts/check, addMembers |

---

## Domain Layer

### Repositories (interfaces)

| Interface | Key methods |
|---|---|
| `SessionRepository` | `sessionState: Flow<SessionState>`, startAuth, verifyAuth, updateDisplayName, signOut |
| `GroupRepository` | observeGroupsWithUnread, observeGroup, refreshGroups, createGroup, updateGroup, deleteGroup, muteGroup, leaveGroup |
| `MessageRepository` | observeThread, sendMessage, loadOlderMessages, markRead |
| `ContactsRepository` | checkContacts, addMembersToGroup |
| `ContactsLookup` | `nameFor(phoneE164): String?`, `allPhoneNumbers(): List<String>` |
| `LegalRepository` | getDocument(key), markSeen(key) |
| `SyncController` | start(), stop(), refreshNow(), `syncState: Flow<SyncState>` |

### SyncState

| State | Meaning |
|---|---|
| `Idle` | Not started (signed out / backgrounded) |
| `Connecting` | WS handshake in progress |
| `Connected` | Live; WS active |
| `BackingOff` | Reconnect delay after failure |
| `UpdateRequired` | Server demanded client update (WS error frame) |

### Use Cases

| Use Case | Returns | Notes |
|---|---|---|
| `ValidateDisplayNameUseCase` | `TextValidation` | Trims, checks non-empty, max 64 code-points |
| `ValidateGroupNameUseCase` | `TextValidation` | Max 100 code-points |
| `ValidateMessageBodyUseCase` | `TextValidation` | Max 2000 code-points |
| `CanPostUseCase` | `Boolean` | Checks myRole vs whoCanPost |
| `CanAddMembersUseCase` | `Boolean` | Checks myRole vs whoCanAddMembers |
| `ObserveThreadUseCase` | `Flow<List<LabeledThreadItem>>` | Merges messages + outbox items |
| `ResolveSenderLabelUseCase` | `SenderLabel` | Contacts lookup → name, else displayName, else phone |

### TextValidation

| Type | Fields |
|---|---|
| `TextValidation.Valid` | `text: String` (trimmed) |
| `TextValidation.Empty` | — |
| `TextValidation.TooLong` | `count: Int, max: Int` |

### ThreadItem (domain)

| Type | Fields |
|---|---|
| `ThreadItem.Incoming` | `message: Message` |
| `ThreadItem.Outgoing` | `message: Message, state: OutgoingState` |
| `ThreadItem.SystemReply` | command reply shown inline |

**OutgoingState:**

| State | Meaning |
|---|---|
| `Pending` | In outbox, not yet acknowledged by server |
| `Sent` | Server confirmed (EchoConfirmation received) |
| `Failed` | Error code set on outbox row |

### AndroidContactsLookup

Reads Android device contacts on `Dispatchers.IO`. Requires `READ_CONTACTS` permission. Both `nameFor()` and `allPhoneNumbers()` return empty/null gracefully without the permission — no crash, no prompt.

### GroupKind enum

`STANDARD` | `SYSTEM` | `UNKNOWN`

- `SYSTEM` groups: `ComposerUiState.Hidden` — no text input ever rendered
- `STANDARD` groups: normal chat flow with permission checks
