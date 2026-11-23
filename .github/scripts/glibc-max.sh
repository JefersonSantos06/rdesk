#!/bin/sh
# Fails if a binary needs a glibc newer than 2.35 (Ubuntu 22.04; Debian 12 has 2.36): it would
# not start there.
#
#   .github/scripts/glibc-max.sh target/release/rdesk [more binaries…]
set -eu
max=2.35
for bin in "$@"; do
  need=$(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
  echo "$bin needs glibc $need"
  # In order (sort -C) only when it is $max or older.
  if ! printf '%s\n' "$need" "$max" | sort -V -C; then
    echo "::error::$bin needs glibc $need: it would not run on Ubuntu 22.04 / Debian 12"
    exit 1
  fi
done
