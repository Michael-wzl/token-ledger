# Security model

TokenLedger trusts the local user, the executable, and its own private data directory. Other websites and imported files are untrusted. A malicious process running as the same OS user is outside the security boundary: it can already read source transcripts and the counter's private database.

The runtime needs source-file reads, writes inside its own directory, and a loopback HTTP listener. `--open` additionally launches the OS browser opener at the user's request. No AI executable, credential store, vendor endpoint, or outbound network connection is required. Build tools download public dependencies separately from runtime collection.

## Enforced controls

- Source opens are read-only, with symlink checks and fixed discovery roots. Caller-supplied remote-log roots are explicit CLI options.
- Claude IndexedDB is parsed as byte files. No database writer, lock, authentication store, or browser profile API is used.
- The database directory must be private and must not overlap an AI source tree. Database-file symlinks are rejected.
- The server binds to `127.0.0.1` only. API access requires a fresh random 256-bit bearer token. Host/Origin checks and same-origin fetch restrictions reject rebinding and cross-site requests.
- A restrictive Content Security Policy blocks external assets, framing, objects, and cross-origin connections. Untrusted labels are rendered as text.
- Import request bodies are bounded. Raw transcripts and import contents are processed in memory and never persisted. Memory may still be subject to ordinary OS paging/crash behavior.
- Parser failures report coverage gaps without printing offending transcript lines. No usage is guessed from text or quota percentages.

## Limits

Internal provider log/cache formats can change. A decoded record proves observed counters, not account-wide completeness or final billing. Source retention, cloud-only tasks, absent timestamps, account ambiguity, concurrent cache updates, and invalid records can reduce coverage. The dashboard reports these gaps.

Safety tests verify source snapshots, protected writes, authorization checks, and offline execution. They do not constitute a formal proof or an independent security audit. The application itself does not impose an OS sandbox; an OS policy can additionally restrict filesystem and network access. Do not run it with elevated privileges.

Report suspected credential access, source writes, outbound requests, or accounting inflation through a private report before sharing sensitive traces. Never include credentials or full transcripts in an issue.
