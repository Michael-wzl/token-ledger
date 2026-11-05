# Parser provenance

TokenLedger reuses parser work from [Tokscale](https://github.com/junhoyeo/tokscale), pinned to **v4.21.0**, commit **b26c433d03f23a2255ad9f212512ce83ede83da3**. It does not execute the upstream CLI or import its networking, authentication, pricing, ranking, configuration or persistent cache layers.

The following files are vendored from `crates/tokscale-core/src/claude_desktop/` at that commit, with TokenLedger's source-symlink checks added to `indexeddb.rs`:

- `src/claude_desktop/indexeddb.rs`: read-only Chromium IndexedDB framing, manifest snapshot validation, external blobs and Snappy values.
- `src/claude_desktop/manifest.rs`: CURRENT/MANIFEST selection and CRC-validated WAL records; obsolete SSTs/WALs are excluded.
- `src/claude_desktop/v8.rs`: bounded V8/Blink structured-clone deserializer.
- `src/claude_desktop/LICENSE-v8`: the decoder's Apache-2.0 license.

`src/claude_desktop/mod.rs` adapts the upstream Desktop usage reconciliation into TokenLedger's own record/coverage types. Local changes preserve opaque account scopes and product provenance, retain undated cumulative balances, and merge timestamped transcript/cache requests before subtracting cumulative lower bounds. Only the explicit macOS Claude conversation-cache root is read. The decoder's existing source/local-change attribution is retained in `v8.rs`.

`src/parsers.rs` and `src/import.rs` adapt accounting algorithms and column semantics from the pinned `sessions/codex.rs`, `sessions/claudecode.rs` and `sessions/cursor.rs`; they do not copy the complete upstream modules. Retained behavior includes Codex pending-response confirmation, late-settlement/compaction handling, cumulative-snapshot and fork-replay gates; Claude streaming completeness, bare-input versus split-input handling and advisor iterations; and vendor CSV v1-v3 token-column semantics. Local integration uses hashed account/session/request identities, explicit source inputs, recorded numeric counters and coverage diagnostics, and excludes character-based estimates and all network/authentication code.

One intentional accounting correction beyond the pinned upstream cross-source merge: a cache-bearing Claude snapshot wins over a bare whole-prompt snapshot even when ordinary input is explicitly zero. Treating zero as missing and taking the bare input maximum would count the cached prompt twice. If a partial snapshot omitted ordinary input, retaining zero is a conservative lower bound under the source's partial-coverage status.

Request deduplication also quarantines contradictory known account scopes or model labels for a logical ID, returns a numeric conflict count, and exposes source diagnostics. Later copies in the same batch cannot reinstate the quarantined request. Unknown metadata may be upgraded from matching request evidence; `[1m]` context-window suffixes are normalized for model comparison. Codex cached/reasoning refinements preserve the inclusive input/output total instead of adding a response.

Tokscale's MIT copyright and license are preserved at `vendor/tokscale/LICENSE`. The decoder retains its additional Apache-2.0 attribution and license. The `leveldb-core` dependency is pinned to 0.1.2 and used only through `parse_log_bytes` / `parse_table_bytes`: TokenLedger never opens LevelDB through a database writer or acquires its LOCK.

The synthetic tests accompanying these readers come from the same pinned upstream files. They cover manifest deletions, stale-file exclusion, failed/changed snapshots, tombstones, external blobs, compressed wrappers, corrupted lengths and structured-clone resource limits. They contain no user conversations or account data.

TokenLedger rejects symlinks within Claude's cache roots and walks canonical source paths using descriptor-relative, read-only `openat` with `O_NOFOLLOW` on Unix. The integration rejects symlinked Claude application/cache ancestors before opening the store. This prevents file/parent symlinks from redirecting source reads to credentials or unrelated files; no application database connection is created.

Internal Claude formats can change. A successful parse establishes recorded local usage, not complete account-wide history; malformed, truncated, unavailable and undated data stay visible in coverage diagnostics.
