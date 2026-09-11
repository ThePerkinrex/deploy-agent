#!/usr/bin/env bash
# Generates a local CA + a server cert for deploy-agent's rustls listener.
#
# Edit these two before running:
#   TAILNET_DOMAIN — the Pi's MagicDNS name, e.g. rpi.foxtrot-lambda.ts.net
#   TAILSCALE_IP    — the Pi's Tailscale 100.x.x.x IP (tailscale ip -4 on the Pi)
set -euo pipefail

TAILNET_DOMAIN="localhost"
TAILSCALE_IP="127.0.0.1"

OUT_DIR="./certs"
mkdir -p "$OUT_DIR"
cd "$OUT_DIR"

echo "==> 1. CA private key (keep ca-key.pem offline — never copy it to the Pi or CI)"
openssl genrsa -out ca-key.pem 4096
chmod 600 ca-key.pem

echo "==> 2. Self-signed CA certificate (public — this is what deploy-ci --ca-cert trusts)"
openssl req -x509 -new -nodes \
  -key ca-key.pem \
  -sha256 \
  -days 3650 \
  -subj "/CN=deploy-agent local CA" \
  -out ca-cert.pem

echo "==> 3. Server private key (stays on the Pi, mode 600, referenced by deploy-agent's rustls config)"
openssl genrsa -out server-key.pem 2048
chmod 600 server-key.pem

echo "==> 4. SAN config for the CSR (MagicDNS name + Tailscale IP)"
cat > san.cnf <<EOF
[req]
distinguished_name = req_distinguished_name
req_extensions = v3_req
prompt = no

[req_distinguished_name]
CN = ${TAILNET_DOMAIN}

[v3_req]
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = @alt_names

[alt_names]
DNS.1 = ${TAILNET_DOMAIN}
IP.1 = ${TAILSCALE_IP}
EOF

echo "==> 5. CSR"
openssl req -new \
  -key server-key.pem \
  -out server.csr \
  -config san.cnf

echo "==> 6. Sign the CSR with the CA (SANs must be re-passed via -extfile — the CSR's own extensions are ignored by 'openssl x509 -req')"
openssl x509 -req \
  -in server.csr \
  -CA ca-cert.pem -CAkey ca-key.pem -CAcreateserial \
  -out server-cert.pem \
  -days 825 -sha256 \
  -extfile san.cnf -extensions v3_req

echo "==> 7. Sanity checks"
openssl verify -CAfile ca-cert.pem server-cert.pem
echo "--- SANs on the issued cert: ---"
openssl x509 -in server-cert.pem -noout -text | grep -A1 "Subject Alternative Name"

rm -f server.csr san.cnf ca-cert.srl

echo ""
echo "Done. Files in ${OUT_DIR}:"
echo "  ca-cert.pem     -> public. Copy to CI as the file passed to 'deploy-ci send --ca-cert'"
echo "  ca-key.pem      -> SECRET. Do not copy anywhere. Only needed to issue/renew server certs."
echo "  server-cert.pem -> goes on the Pi, referenced by deploy-agent's rustls cert config"
echo "  server-key.pem  -> SECRET, goes on the Pi at mode 600, referenced by deploy-agent's rustls key config"