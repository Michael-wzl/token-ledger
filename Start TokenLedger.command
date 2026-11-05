#!/bin/sh
set -eu
project_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$project_dir"
if [ ! -x ./token-ledger ]; then
  printf '%s\n' 'Build the portable executable first: scripts/build.sh'
  exit 1
fi
exec ./token-ledger --data-dir "$project_dir/data" serve --open
