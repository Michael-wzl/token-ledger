#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
cargo build --release --locked
cp target/release/token-ledger ./token-ledger
printf '%s\n' 'Ready: ./token-ledger serve --open'
