#!/usr/bin/env bash
# Issue and install a Let's Encrypt certificate for a public IP or hostname.
#
# For an IP address, Let's Encrypt requires the `shortlived` profile. Those
# certificates are valid for roughly six days, so acme.sh must check renewal
# frequently; `--days 3` makes it renew with about three days remaining.
#
# For a DNS hostname this uses the normal profile with a 30-day renewal window.
#
# Requires: acme.sh, nginx serving /var/www/letsencrypt on port 80, and inbound
# TCP 80 reachable from the public internet.
set -euo pipefail

NAME="${1:-${UNDERHAVEN_IP:-}}"
if [[ -z "$NAME" ]]; then
  echo "usage: $0 <public-ipv4-or-ipv6-or-hostname>" >&2
  exit 2
fi

ACME="${ACME_SH:-/root/.acme.sh/acme.sh}"
WEBROOT="${WEBROOT:-/var/www/letsencrypt}"
TLS_DIR="${TLS_DIR:-/etc/underhaven/tls}"
ACME_SERVER="${ACME_SERVER:-letsencrypt}"
NGINX_GROUP="${NGINX_GROUP:-www-data}"

if [[ ! -x "$ACME" ]]; then
  echo "acme.sh not found at $ACME (set ACME_SH)" >&2
  exit 2
fi

install -d -m 0755 "$WEBROOT/.well-known/acme-challenge"
install -d -o root -g "$NGINX_GROUP" -m 0750 "$TLS_DIR"

# IP addresses need the short-lived profile; DNS names use the standard one.
if [[ "$NAME" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ || "$NAME" == *:* ]]; then
  echo "==> requesting Let's Encrypt shortlived profile for IP $NAME"
  PROFILE_ARGS=(--certificate-profile shortlived --days 3)
else
  echo "==> requesting Let's Encrypt certificate for $NAME"
  PROFILE_ARGS=(--days 30)
fi

"$ACME" --issue \
  -d "$NAME" \
  -w "$WEBROOT" \
  --server "$ACME_SERVER" \
  "${PROFILE_ARGS[@]}" \
  --keylength ec-256 || ISSUE_STATUS=$?
# acme.sh returns non-zero when a certificate is not due for renewal. That is
# not a failure: install the stored certificate below. A real issuance failure
# also lands here, and `--install-cert` then fails because no cert exists.
if [[ "${ISSUE_STATUS:-0}" -ne 0 ]]; then
  echo "==> acme.sh issue returned ${ISSUE_STATUS}; checking for an existing stored certificate"
fi

echo "==> installing certificate into $TLS_DIR"
"$ACME" --install-cert -d "$NAME" --ecc \
  --key-file "$TLS_DIR/key.pem" \
  --fullchain-file "$TLS_DIR/fullchain.pem" \
  --reloadcmd "systemctl reload nginx"

chmod 0644 "$TLS_DIR/fullchain.pem"
chown root:"$NGINX_GROUP" "$TLS_DIR/fullchain.pem" "$TLS_DIR/key.pem"
# nginx workers (www-data) must be able to read the key, but not other users.
chmod 0640 "$TLS_DIR/key.pem"

echo "==> certificate summary"
openssl x509 -in "$TLS_DIR/fullchain.pem" -noout -subject -issuer -dates -ext subjectAltName

echo
if [[ "$NAME" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ || "$NAME" == *:* ]]; then
  echo "Renewal: acme.sh stores the shortlived profile and --days 3 in its"
  echo "renewal configuration. Its daily cron job is sufficient for a ~6 day"
  echo "certificate. Verify with: $ACME --list"
else
  echo "Renewal: acme.sh installs a daily cron job. Verify with: $ACME --list"
fi
