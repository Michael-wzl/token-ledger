# TokenLedger

A local AI token dashboard that reads usage records without changing the tools that produced them. Open it in your browser; no native app, extension, account, or background service is required.

**Counts are observed usage, not a complete account bill.** Missing data stays unknown. Claude Cowork and Grok Bot can run in the cloud without retaining a complete token history on your Mac.

## Run

With the portable executable in this folder:

```sh
./token-ledger serve --open
```

Or double-click `Start TokenLedger.command` on macOS. Stop the terminal process with Ctrl+C. The dashboard is available only on `127.0.0.1`, uses a fresh access token at every launch, and refreshes local collection every five minutes while running. Keep its launch URL private.

The executable defaults to a private `data/` directory beside itself. To choose another dedicated directory:

```sh
./token-ledger --data-dir /path/to/private-counter-data serve --open
```

The database contains token counts, timestamps, model/project labels, opaque record IDs, and coverage/import metadata. It does not contain prompts, responses, credentials, or copied transcripts. `data/`, build products, local test output, and exports are ignored by Git.

## What is counted

- **Codex:** active and archived local session logs, with response-level and cumulative usage reconciliation and subagent/fork handling.
- **Claude:** Claude Code/retained Desktop transcripts and recorded usage in Desktop's IndexedDB cache. Retained Code and Cowork records are supported; a cache is not a complete account history. Undated cumulative balances are displayed separately from daily totals.
- **Cursor:** manually imported usage-event CSV/JSON exports. No session credentials are read and no vendor request is sent.
- **Grok Bot:** its cached quota snapshot is shown separately. Tokens require an export with numeric token fields and explicit Grok Bot attribution. Text length, context fill, and quota percentages are never converted into claimed token counts.

Use the import dialog for vendor exports or remote-machine JSONL logs. Choose the account label and inclusive date window. JSON timestamps must carry a timezone or be numeric epoch timestamps; ambiguous timestamps are rejected. Unknown account attribution remains unknown. Use consistent labels when importing the same account.

Imports replace an earlier import with the **same app, account, and exact date window**. Partially overlapping windows are rejected. Within imported app windows, overlapping local records are held out of combined totals because their account/request overlap may be uncertain. Those local records remain in the ledger. This conservative rule can undercount multiple accounts; the dashboard reports the held records rather than silently adding them twice.

```sh
./token-ledger import usage.csv --app cursor --account personal --from 2026-11-01 --to 2026-11-05
./token-ledger --source-root /path/to/copied-remote-logs scan
mkdir -p exports
./token-ledger export > exports/numeric-history.json
```

Imports are limited to 32 MiB. Exported history includes retained records and coverage; it is a numeric audit export, not an additional provider usage import. Sessions no longer present in upstream logs retain their numeric history. Present sessions are reconciled as snapshots so late confirmations cannot duplicate older record identities; a truncated replacement may reduce observed coverage. Undated balances are replaceable source snapshots. Default day boundaries are midnight in `Asia/Hong_Kong`; the dashboard preserves UTC timestamps for aggregation.

## Safety boundary

Collectors open approved source files read-only. They never install hooks, launch AI CLIs, access Keychain/authentication stores, open application databases through a database writer, acquire source database locks, or create source-side caches. Source symlinks are rejected. Claude's IndexedDB parser reads manifest-selected bytes directly, including external blobs, without opening a LevelDB writer.

The server has no outbound HTTP client, telemetry, login, token-refresh, leaderboard, or cloud-sync functionality. All web assets are embedded. API access requires the launch token, the expected Host, and same-origin requests; imports and scans also require an exact Origin. These checks protect against cross-origin websites and DNS rebinding. The listener is loopback-only.

The counter writes only its own private database directory. A data directory overlapping an AI-tool source is rejected. It does not alter AI Token League or stop that app's existing synchronization. No vendor access is enabled in this version.

See [SECURITY.md](SECURITY.md) for the threat model and verification limits, and [docs/UPSTREAM.md](docs/UPSTREAM.md) for the pinned Tokscale sources and licenses.

## Build and test

Rust stable is needed only to build. Dependencies are pinned by `Cargo.lock`; building initially downloads crates. The finished executable operates offline.

```sh
cargo test --locked
cargo build --release --locked
cp target/release/token-ledger ./token-ledger
```

Or run `scripts/build.sh`. No frontend package manager or Node runtime is required. Meaningful parser, accounting, safety, and HTTP boundary tests are included. Do not use real account transcripts as committed test fixtures.

## License

TokenLedger is MIT licensed. Adapted Tokscale code is pinned to v4.21.0 (`b26c433d03f23a2255ad9f212512ce83ede83da3`) and retains its MIT notice. The vendored V8 decoder retains its Apache-2.0 license and upstream attribution. Dependency licenses remain applicable.
