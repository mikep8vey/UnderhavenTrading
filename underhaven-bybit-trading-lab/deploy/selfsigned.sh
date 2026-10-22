#!/usr/bin/env bash
# SELF-SIGNED FALLBACK. The resulting certificate is not publicly trusted.
#
# Usage: ./selfsigned.sh <public-ip-or-hostname>
# If no argument is given, the name is read from UNDERHAVEN_IP or from
# UNDERHAVEN_PUBLIC_URL in /etc/underhaven/underhaven.env.
set -euo pipefail

NAME="${1:-${UNDERHAVEN_IP:-}}"
if [[ -z "$NAME" && -f /etc/underhaven/underhaven.env ]]; then
  NAME="$(sed -n 's#^UNDERHAVEN_PUBLIC_URL=https://##p' /etc/underhaven/underhaven.env \
    | head -n 1 | sed 's#/.*$##')"
fi
if [[ -z "$NAME" ]]; then
  echo "usage: $0 <public-ip-or-hostname>" >&2
  exit 2
fi

# Use an IP SAN for addresses and a DNS SAN for hostnames. A certificate with
# the wrong SAN type is rejected by browsers even if the CN matches.
if [[ "$NAME" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ || "$NAME" == *:* ]]; then
  SAN="IP:$NAME"
else
  SAN="DNS:$NAME"
fi

TLS_DIR="${TLS_DIR:-/etc/underhaven/tls}"
NGINX_GROUP="${NGINX_GROUP:-www-data}"
install -d -o root -g "$NGINX_GROUP" -m 0750 "$TLS_DIR"

openssl req -x509 -newkey rsa:3072 -sha256 -days 365 -nodes \
  -keyout "$TLS_DIR/selfsigned.key" \
  -out "$TLS_DIR/selfsigned.crt" \
  -subj "/CN=$NAME/O=UNDERHAVEN SELF-SIGNED FALLBACK" \
  -addext "subjectAltName=$SAN" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
  -addext "extendedKeyUsage=serverAuth"

chown root:"$NGINX_GROUP" "$TLS_DIR/selfsigned.key" "$TLS_DIR/selfsigned.crt"
chmod 0640 "$TLS_DIR/selfsigned.key"
chmod 0644 "$TLS_DIR/selfsigned.crt"

if [[ -f /etc/underhaven/underhaven.env ]]; then
  sed -i 's/^UNDERHAVEN_TLS_MODE=.*/UNDERHAVEN_TLS_MODE=self_signed/' \
    /etc/underhaven/underhaven.env
fi

if systemctl is-active --quiet nginx; then
  systemctl reload nginx
fi

echo "SELF-SIGNED certificate written to $TLS_DIR (365 days)."
echo "This certificate is NOT publicly trusted. The dashboard will label it"
echo "as self-signed. Install a trust exception only on devices you control,"
echo "or fix Let's Encrypt short-lived issuance."
