#!/bin/sh
# PipeWire 0.3.65 headers (Debian 12's) for building rdesk on Ubuntu 22.04, whose own (0.3.48)
# are too old for the libspa crate. Only the headers: the program still links against the
# system's libpipewire-0.3.so (0.3.48), so needing a function newer than that fails the link
# here instead of the start on Ubuntu 22.04. The headers are the same on every processor.
#
#   export PKG_CONFIG_PATH="$(.github/scripts/pipewire-headers.sh "$RUNNER_TEMP/pipewire")"
#
# Needs curl, dpkg-deb and the system's libpipewire-0.3-dev (for the library to link with).
set -eu
dir=$1
version=0.3.65-3+deb12u1
mkdir -p "$dir/debs" "$dir/root" "$dir/pkgconfig"

# name, sha256. A file gone from the Debian mirror (a newer bookworm update) is still on
# snapshot.debian.org.
fetch() {
  file="$1_${version}_amd64.deb"
  out="$dir/debs/$file"
  for base in https://deb.debian.org/debian https://snapshot.debian.org/archive/debian/20261120T000000Z; do
    curl -fsSL --retry 3 -o "$out" "$base/pool/main/p/pipewire/$file" && break
  done
  echo "$2  $out" | sha256sum -c --quiet >&2
  dpkg-deb -x "$out" "$dir/root"
}
fetch libpipewire-0.3-dev 5dd9a75ea9d09791b38672b338a57a1b216727c94dcbb78b15a12ab411e99ff5
fetch libspa-0.2-dev 22350821fe3abe1748240b5f45b5797cbcff6a6a9e4ff0dc4d3ac4d866c34894

include="$dir/root/usr/include"
cat > "$dir/pkgconfig/libspa-0.2.pc" <<EOF
Name: libspa
Description: Simple Plugin API (Debian 12's headers)
Version: 0.2
Cflags: -I$include/spa-0.2 -D_REENTRANT
EOF
cat > "$dir/pkgconfig/libpipewire-0.3.pc" <<EOF
Name: libpipewire
Description: PipeWire 0.3.65 headers with the system's library
Version: 0.3.65
Requires: libspa-0.2
Libs: -lpipewire-0.3
Cflags: -I$include/pipewire-0.3 -D_REENTRANT
EOF
echo "$dir/pkgconfig"
