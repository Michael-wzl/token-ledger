# Validation record

Verified locally on macOS / Apple Silicon on 2026-11-05:

- `cargo test --offline --locked`: **71 tests passed**. Coverage includes byte decoding, manifest selection, cache/transcript overlap, explicit zero-input splits, Codex cumulative refinements and forks, imports, dates, account conflicts, source immutability, symlink/hardlink rejection, and HTTP authorization boundaries.
- The optimized executable passed `python3 scripts/smoke.py ./token-ledger`: live loopback HTTP, Host/Origin/auth rejection, repeated imports, consistent hourly buckets, raw-content exclusion from SQLite files, and unchanged synthetic source contents, modification times, and inventory.
- The optimized executable completed a real-source scan under a macOS test sandbox denying network operations, child-process creation, writes outside the counter's own test database, and reads of known credential stores. The test policy uses local absolute paths and is deliberately not committed.
- Desktop and 390-pixel mobile browser layouts were visually inspected. Date ranges, source filtering, reset controls, and the import dialog worked. The browser reported no console warnings/errors and requested only localhost assets/APIs.
- The release executable is ARM64 Mach-O and links only standard macOS system libraries. The compiled executable, private database, local profiles, and screenshots are excluded from Git.

## Repeat

```sh
cargo test --locked
cargo build --locked
python3 scripts/smoke.py
cargo build --release --locked
python3 scripts/smoke.py target/release/token-ledger
```

The checked-in workflow runs tests and the synthetic HTTP smoke check on macOS and Linux. The real-source sandbox check is additional local evidence, not a formal security proof. Source formats and record availability can change; complete Cowork/Grok Bot token coverage is not established by these tests.
