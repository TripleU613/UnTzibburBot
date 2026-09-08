# Security

- Report vulnerabilities privately to the repository owner rather than in a public issue.
- The bridge holds users' Tzibbur session tokens (AES-256-GCM encrypted with `BRIDGE_MASTER_KEY`) so it can stay connected for them. Treat the host, the Directus database and the master key as sensitive.
- Message text is never stored at rest by the bridge (see `crates/bridge/README.md`, "Privacy & threat model").
- No secrets live in this repository. Runtime configuration comes from a `.env` file on the host; see `.env.example`.
