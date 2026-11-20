#!/bin/bash
#
# Usage: clang-format.sh [<arg-to-clang-format> ...]
# Examples:
#   clang-format.sh --dry-run  Don't make changes, just print
#   clang-format.sh            Format all non-vendored .h and .c files
#
set -euo pipefail
cd "$(dirname "$0")"
SCRIPTS_DIR=$PWD
cd ..

command -v clang-format >/dev/null || {
    echo "$0: clang-format not found in PATH" >&2
    exit 1
}

clang-format --Werror --style="file:$SCRIPTS_DIR/clang-format.yaml" -i "$@" \
    $(ls playbit-api/include/playbit/sys/*.h \
         playbit/include/playbit/*.h \
         |
        git check-attr --stdin linguist-generated linguist-vendored |
        grep -F ': unspecified' | cut -d: -f1 | sort -u)
