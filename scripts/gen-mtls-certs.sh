#!/usr/bin/env bash
# Generate a self-signed mTLS chain (CA + server + one client `.p12`)
# suitable for pointing a real ATAK device at a tak-rs dev server.
#
# Usage:
#   ./scripts/gen-mtls-certs.sh <out-dir> <server-host>
#
# Example:
#   ./scripts/gen-mtls-certs.sh /tmp/tak-certs my-takrs.lan
#
# Outputs:
#   ca.pem / ca.key             — root CA (install on ATAK as trusted)
#   server.pem / server.key     — what tak-rs presents
#   server.p12                  — same, packaged for tools that want PKCS12
#   client-VIPER01.p12          — client identity for ATAK
#                                  (default password: atakatak)
#
# This is bench / dev only. Production deployments should use a real
# CA, hardware-backed keys, and not commit any of this output.

set -euo pipefail

OUT_DIR="${1:-}"
SERVER_HOST="${2:-}"
P12_PASS="${P12_PASS:-atakatak}"
DAYS="${DAYS:-825}"   # apple's max cert lifetime; iOS will reject longer

if [[ -z "$OUT_DIR" || -z "$SERVER_HOST" ]]; then
    echo "Usage: $0 <out-dir> <server-host>" >&2
    echo "Example: $0 /tmp/tak-certs takrs.local" >&2
    exit 64
fi

if ! command -v openssl >/dev/null 2>&1; then
    echo "openssl not on PATH" >&2
    exit 127
fi

mkdir -p "$OUT_DIR"
cd "$OUT_DIR"

echo "==> generating CA in $OUT_DIR"
openssl genrsa -out ca.key 4096 2>/dev/null
openssl req -x509 -new -nodes -key ca.key -sha256 -days "$DAYS" \
    -subj "/CN=tak-rs-dev-ca/O=tak-rs/OU=conformance" \
    -out ca.pem 2>/dev/null

# --- server ---
echo "==> generating server cert for CN=$SERVER_HOST"
openssl genrsa -out server.key 4096 2>/dev/null
openssl req -new -key server.key \
    -subj "/CN=$SERVER_HOST/O=tak-rs/OU=server" \
    -out server.csr 2>/dev/null

# SAN block — ATAK validates SAN, not just CN. Cover both
# DNS-style hostnames and bare IPs.
SAN_BLOCK=$(mktemp)
trap 'rm -f "$SAN_BLOCK"' EXIT
{
    echo "subjectAltName = @alt_names"
    echo "[alt_names]"
    if [[ "$SERVER_HOST" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        echo "IP.1 = $SERVER_HOST"
    else
        echo "DNS.1 = $SERVER_HOST"
    fi
    echo "DNS.2 = localhost"
    echo "IP.2 = 127.0.0.1"
} > "$SAN_BLOCK"

openssl x509 -req -in server.csr \
    -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out server.pem -days "$DAYS" -sha256 \
    -extfile "$SAN_BLOCK" 2>/dev/null
rm -f server.csr ca.srl

# Bundle the server key+cert into a PKCS12 for embedded uses.
openssl pkcs12 -export \
    -inkey server.key -in server.pem -certfile ca.pem \
    -name "tak-rs-server" \
    -passout "pass:$P12_PASS" \
    -out server.p12 2>/dev/null

# --- client (VIPER01) ---
echo "==> generating client cert (CN=VIPER01)"
openssl genrsa -out client-VIPER01.key 4096 2>/dev/null
openssl req -new -key client-VIPER01.key \
    -subj "/CN=VIPER01/O=tak-rs/OU=client" \
    -out client-VIPER01.csr 2>/dev/null

CLIENT_EXT=$(mktemp)
trap 'rm -f "$SAN_BLOCK" "$CLIENT_EXT"' EXIT
echo "extendedKeyUsage = clientAuth" > "$CLIENT_EXT"

openssl x509 -req -in client-VIPER01.csr \
    -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out client-VIPER01.pem -days "$DAYS" -sha256 \
    -extfile "$CLIENT_EXT" 2>/dev/null
rm -f client-VIPER01.csr ca.srl

openssl pkcs12 -export \
    -inkey client-VIPER01.key -in client-VIPER01.pem -certfile ca.pem \
    -name "VIPER01" \
    -passout "pass:$P12_PASS" \
    -out client-VIPER01.p12 2>/dev/null

# Tighten file modes on the secret material.
chmod 600 ca.key server.key client-VIPER01.key server.p12 client-VIPER01.p12

echo
echo "Output:"
ls -la "$OUT_DIR"
echo
cat <<EOF
Next steps:
  1. Copy ca.pem and client-VIPER01.p12 to the ATAK device.
     - On Android: Settings -> Security -> Install from storage.
     - In ATAK: Settings -> Network -> Quick Connect SSL/TLS Setup.
  2. P12 password is: $P12_PASS  (override with P12_PASS=... when running this script)
  3. Boot tak-rs with the server cert. The mTLS-on-:8089 wiring in
     the firehose is a known gap; until it lands you can verify the
     cert chain with:
         openssl verify -CAfile ca.pem server.pem
         openssl verify -CAfile ca.pem client-VIPER01.pem
EOF
