#!/bin/sh
# Type-check the web client: the page against DOM types, the workers against
# worker types. Shared modules must satisfy both.
set -u
web=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
[ -x "$web/node_modules/.bin/tsc" ] || npm ci --prefix "$web" --no-audit --no-fund
status=0
"$web/node_modules/.bin/tsc" -p "$web/tsconfig.json" || status=1
"$web/node_modules/.bin/tsc" -p "$web/tsconfig.worker.json" || status=1
exit $status
