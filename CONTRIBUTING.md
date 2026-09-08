# Contributing

Thanks for helping. This is a small, opinionated project; here is what makes a change easy to take.

## Ground rules

- Be kind. See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
- Rust only.
- Never add code that stores or logs message text, phone numbers, or tokens. The privacy model in
  [`crates/bridge/README.md`](crates/bridge/README.md) is a hard constraint, not a preference.
- No secrets, hostnames, IPs, or personal data in the repository, including tests and examples.
- Bot-facing text: short sentences, no emojis, no marketing.
- By contributing you agree your work is licensed under the AGPL-3.0-or-later like the rest of the project.

## Setup

```sh
rustup update stable
cp .env.example .env               # only needed to run the bot
cargo build --workspace
cargo test --workspace             # unit tests + a mock Tzibbur/Telegram server
```

Running against a real Directus and a real Tzibbur account is described in [DEVELOPMENT.md](DEVELOPMENT.md).

## Before you open a pull request

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs exactly these plus a Docker build. Keep pull requests focused; one topic per PR.
Explain *why* in the description; the diff already says *what*.

## What gets merged

- Green CI is required. `main` is protected by a ruleset.
- Dependabot updates (patch and minor) merge automatically when CI is green.
- Other pull requests merge automatically once a maintainer adds the `automerge` label, or are
  merged by hand after review. Anything touching auth, storage, or the privacy model gets a review.
- Squash merges only; the branch is deleted afterwards.

## Reporting bugs and ideas

Use the issue templates. For security problems do **not** open a public issue; see
[SECURITY.md](SECURITY.md).
