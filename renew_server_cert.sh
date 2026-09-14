#!/usr/bin/env bash
# Re-issues just the server cert/key from the EXISTING local CA produced by
# create_certs.sh — use this to add/change SANs (e.g. you added a DNS name
# or the Tailscale IP changed) without generating a new CA and having to
# re-distribute ca-cert.pem to every downstream repo's DEPLOY_CA_CERT secret.
#
# Edit SAN_DNS_NAMES / SAN_IPS below to whatever hostnames/IPs the agent
# needs to be reachable as. Requires ca-cert.pem and ca-key.pem to already
# exist in OUT_DIR (run create_certs.sh once first if they don't).
set -euo pipefail

SAN_DNS_NAMES=("raspberrypi" "localhost")
SAN_IPS=("127.0.0.1")

OUT_DIR="./certs"
cd "$OUT_DIR"

if [[ ! -f ca-cert.pem || ! -f ca-key.pem ]]; then
  echo "error: ca-cert.pem / ca-key.pem not found in ${OUT_DIR} — run create_certs.sh first" >&2
  exit 1
fi

echo "==> 1. New server private key (stays on the Pi, mode 600)"
openssl genrsa -out server-key.pem 2048
chmod 600 server-key.pem

echo "==> 2. SAN config for the CSR"
{
  echo "[req]"
  echo "distinguished_name = req_distinguished_name"
  echo "req_extensions = v3_req"
  echo "prompt = no"
  echo ""
  echo "[req_distinguished_name]"
  echo "CN = ${SAN_DNS_NAMES[0]}"
  echo ""
  echo "[v3_req]"
  echo "basicConstraints = CA:FALSE"
  echo "keyUsage = digitalSignature, keyEncipherment"
  echo "extendedKeyUsage = serverAuth"
  echo "subjectAltName = @alt_names"
  echo ""
  echo "[alt_names]"
  for i in "${!SAN_DNS_NAMES[@]}"; do
    echo "DNS.$((i + 1)) = ${SAN_DNS_NAMES[$i]}"
  done
  for i in "${!SAN_IPS[@]}"; do
    echo "IP.$((i + 1)) = ${SAN_IPS[$i]}"
  done
} > san.cnf

echo "==> 3. CSR"
openssl req -new \
  -key server-key.pem \
  -out server.csr \
  -config san.cnf

echo "==> 4. Sign the CSR with the existing CA (SANs must be re-passed via -extfile — the CSR's own extensions are ignored by 'openssl x509 -req')"
openssl x509 -req \
  -in server.csr \
  -CA ca-cert.pem -CAkey ca-key.pem -CAcreateserial \
  -out server-cert.pem \
  -days 825 -sha256 \
  -extfile san.cnf -extensions v3_req

echo "==> 5. Sanity checks"
openssl verify -CAfile ca-cert.pem server-cert.pem
echo "--- SANs on the issued cert: ---"
openssl x509 -in server-cert.pem -noout -text | grep -A1 "Subject Alternative Name"

rm -f server.csr san.cnf ca-cert.srl

echo ""
echo "Done. Re-copy the new server cert/key to the Pi (docs/05-...md step 1.3):"
echo "  scp certs/server-cert.pem certs/server-key.pem pi:/tmp/"
echo "  ssh pi 'sudo mv /tmp/server-cert.pem /etc/deploy-agent/tls/cert.pem &&"
echo "           sudo mv /tmp/server-key.pem /etc/deploy-agent/tls/key.pem &&"
echo "           sudo chown deploy-agent:deploy-agent /etc/deploy-agent/tls/*.pem &&"
echo "           sudo chmod 600 /etc/deploy-agent/tls/key.pem &&"
echo "           sudo systemctl restart deploy-agent'"
echo ""
echo "ca-cert.pem is unchanged, so no downstream repo's DEPLOY_CA_CERT secret needs updating."
