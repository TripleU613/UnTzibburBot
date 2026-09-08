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

## What the bridge holds

- Users' Tzibbur session tokens, encrypted with AES-256-GCM under `BRIDGE_MASTER_KEY`, so the bot
  can stay connected on their behalf. Treat the host, the database and the master key as sensitive.
- Ids and mappings only. Message text is erased as soon as it is delivered
  (see `crates/bridge/README.md`, "Privacy & threat model").
- No secrets live in this repository. Runtime configuration comes from a `.env` on the host;
  see `.env.example`.

## Hardening checklist for operators

- Keep `BRIDGE_MASTER_KEY` only in the host `.env` (mode 600) and back it up separately from the database.
- Bind Directus and the bridge port to loopback or a private network; expose the Mini App only through a TLS reverse proxy or Tailscale Funnel.
- Rotate the Tailscale auth key used by the deploy workflow before it expires.
- Set `BRIDGE_ADMIN_TELEGRAM_ID` so error alerts reach you.
