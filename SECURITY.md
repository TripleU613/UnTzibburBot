# Security policy

## Supported versions

Only the latest release and the current `main` branch receive fixes.

## Reporting a vulnerability

Please do **not** open a public issue for security problems.

Use GitHub's private vulnerability reporting on this repository ("Security" tab → "Report a
vulnerability"). You will get an acknowledgement within a few days. Once fixed, the fix is
released and the report is credited unless you prefer otherwise.

In scope: anything that lets one user read or send another user's messages, obtain a session
token or the master key, act on a group they are not a member of, or make the bridge store
message text. Out of scope: the Tzibbur service itself and Telegram.

## What the bridge holds, and how it is protected

- **Message text**: never stored durably. Text passes through memory and a short-lived local
  cache until Telegram or Tzibbur confirms delivery, then it is erased. The cache files themselves
  are SQLCipher databases (AES-256), one per account, each under its own key.
- **Session tokens**: AES-256-GCM, one key per account. Keys are derived with HKDF-SHA256 from
  `BRIDGE_MASTER_KEY`, a purpose label and the account id, so a leaked token blob is useless
  without both the master key and the account it belongs to. The master key never encrypts data
  directly and can be rotated (`BRIDGE_MASTER_KEY_PREVIOUS`) without logging anyone out.
- **Database**: ids and mappings only (Telegram ids, Tzibbur ids, topic ids, sequence numbers).
  Nightly dumps can be encrypted to an `age` public key so the backups are unreadable on the host.
- **Sign-in**: the SMS code the user types is deleted from the chat immediately; requests for
  codes and code attempts are limited per Telegram user and hour.
- **Container**: runs as an unprivileged user with a read-only root filesystem, all Linux
  capabilities dropped and `no-new-privileges`.
- **Transport**: TLS to Telegram and Tzibbur (rustls). Mini App requests are authenticated with
  Telegram's signed `initData`.
- **Repository**: holds no secrets. Runtime configuration comes from a `.env` on the host.

### What encryption cannot do here

The bridge relays between two services that both see message text in the clear (Tzibbur's server
and Telegram's Bot API). End-to-end encryption between Tzibbur users and Telegram is therefore not
possible in this design: the bridge must hold the text in memory for the moment it forwards it.
Whoever controls the host could, in principle, modify the software to keep that text. The
protections above make the stored data useless to a thief and make the operator's promise
checkable in the code, but they do not remove the need to trust the operator. Run your own
instance if you do not.

## Hardening checklist for operators

- Keep `BRIDGE_MASTER_KEY` only in the host `.env` (mode 600) and back it up separately from the database.
- Set `BACKUP_AGE_RECIPIENT` to an `age` public key and keep the private key off the server.
- Rotate the master key now and then: move the current value to `BRIDGE_MASTER_KEY_PREVIOUS`, generate a new `BRIDGE_MASTER_KEY`, restart, then remove the previous key after every account has been touched (a restart re-wraps them all).
- Encrypt the host's disk; the caches and database live in Docker volumes on it.
- Bind Directus and the bridge port to loopback or a private network; expose the Mini App only through a TLS reverse proxy or Tailscale Funnel.
- Rotate the Tailscale auth key used by the deploy workflow before it expires.
- Set `BRIDGE_ADMIN_TELEGRAM_ID` so error alerts reach you.
