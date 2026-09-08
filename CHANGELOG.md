# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow SemVer.

## [Unreleased]

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
