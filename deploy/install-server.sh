#!/bin/bash
# Install (or update) rdesk-server on a Linux VPS with systemd (x86_64 or arm64).
#
#   curl -fsSL https://raw.githubusercontent.com/JefersonSantos06/rdesk/main/deploy/install-server.sh \
#     | sudo bash -s -- --domain rdesk.example.com --admin you@gmail.com
#
# Behind a reverse proxy that already does HTTPS (Caddy, nginx…): add --listen with an
# address only the proxy reaches, e.g. --listen 127.0.0.1:8090, and point the proxy there.
# Run again later to update by hand (the Admin page's "Update the server" does it too).
set -euo pipefail

REPO="JefersonSantos06/rdesk"
DATA=/var/lib/rdesk-server
DOMAIN=""
ADMIN=""
LISTEN=""
while [ $# -gt 0 ]; do
  case "$1" in
    --domain) DOMAIN="$2"; shift ;;
    --admin) ADMIN="$2"; shift ;;
    --listen) LISTEN="$2"; shift ;;
    --repo) REPO="$2"; shift ;;
    *) echo "unknown option $1" >&2; exit 1 ;;
  esac
  shift
done
[ "$(id -u)" = 0 ] || { echo "Run it with sudo." >&2; exit 1; }
command -v systemctl >/dev/null || { echo "This needs systemd." >&2; exit 1; }
case "$(uname -m)" in
  x86_64) PLATFORM=linux-x86_64 ;;
  aarch64|arm64) PLATFORM=linux-aarch64 ;;
  *) echo "No rdesk-server for $(uname -m)." >&2; exit 1 ;;
esac

T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
cd "$T"
# The release's manifest lists every file's SHA-256 (it is signed; the app checks the
# signature, this first install trusts GitHub's HTTPS).
curl -fsSL -o release.json "https://github.com/$REPO/releases/latest/download/release.json"
VERSION=$(sed -n 's/^  "version": "\(.*\)",$/\1/p' release.json)
NAME="rdesk-server-$VERSION-$PLATFORM.tar.gz"
SHA=$(grep -A3 "\"name\": \"$NAME\"" release.json | sed -n 's/.*"sha256": "\([0-9a-f]*\)".*/\1/p')
[ -n "$VERSION" ] && [ -n "$SHA" ] || { echo "Could not read the release manifest." >&2; exit 1; }
echo "Downloading rdesk-server $VERSION ($PLATFORM)…"
curl -fsSL -o "$NAME" "https://github.com/$REPO/releases/download/v$VERSION/$NAME"
echo "$SHA  $NAME" | sha256sum -c --quiet
tar xzf "$NAME"

install -m 755 rdesk-server /usr/local/bin/rdesk-server
install -m 644 rdesk-server.service /etc/systemd/system/
for f in rdesk-server-update.path rdesk-server-update.service; do
  [ -f "$f" ] && install -m 644 "$f" /etc/systemd/system/
done
id rdesk >/dev/null 2>&1 || useradd --system --home-dir "$DATA" --create-home --shell /usr/sbin/nologin rdesk

if [ ! -f "$DATA/server.toml" ]; then
  [ -n "$DOMAIN" ] && [ -n "$ADMIN" ] || { echo "First install: give --domain and --admin." >&2; exit 1; }
  ARGS=(init --public-url "https://$DOMAIN" --admin "$ADMIN")
  [ -n "$LISTEN" ] && ARGS+=(--no-tls --listen "$LISTEN")
  runuser -u rdesk -- env RDESK_SERVER_DATA="$DATA" /usr/local/bin/rdesk-server "${ARGS[@]}"
fi

if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
  [ -z "$LISTEN" ] && ufw allow 443/tcp >/dev/null
  ufw allow 7842/udp >/dev/null
  echo "Firewall (ufw): opened ${LISTEN:+only }${LISTEN:-443/tcp and }7842/udp."
fi

systemctl daemon-reload
[ -f /etc/systemd/system/rdesk-server-update.path ] && systemctl enable --now rdesk-server-update.path >/dev/null
systemctl enable rdesk-server >/dev/null
systemctl restart rdesk-server
sleep 2
systemctl is-active --quiet rdesk-server || { journalctl -u rdesk-server -n 20 --no-pager; exit 1; }
echo
runuser -u rdesk -- env RDESK_SERVER_DATA="$DATA" /usr/local/bin/rdesk-server info
echo
echo "rdesk-server $VERSION is running. Next: the OAuth apps (see the redirect URLs above), then"
echo "  sudo -u rdesk RDESK_SERVER_DATA=$DATA rdesk-server oauth github --client-id … --client-secret …"
echo "  sudo systemctl restart rdesk-server"
