#!/bin/sh
set -eu
cd "$(dirname "$0")"
mkdir -p certs results
if [ -e .env ] || [ -e certs/ca.key ]; then
  printf '%s\n' 'Existing soak secrets found; remove them explicitly before generating replacements.' >&2
  exit 1
fi
umask 077
openssl req -x509 -newkey rsa:2048 -nodes -keyout certs/ca.key -out certs/ca.crt -days 30 -subj '/CN=Rustbox Soak CA' -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign'
openssl req -newkey rsa:2048 -nodes -keyout certs/server.key -out certs/server.csr -subj '/CN=dovecot'
cat > certs/server.ext <<'EXT'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:dovecot,DNS:runner,DNS:localhost,IP:127.0.0.1
EXT
openssl x509 -req -in certs/server.csr -CA certs/ca.crt -CAkey certs/ca.key -CAcreateserial -out certs/server.crt -days 30 -extfile certs/server.ext
python3 - <<'PY'
import base64
import pathlib
import secrets
pathlib.Path('.env').write_text('SOAK_API_KEY=' + secrets.token_hex(32) + '\nSOAK_WEBHOOK_SECRET=' + secrets.token_hex(32) + '\nSOAK_ENCRYPTION_KEY=' + base64.b64encode(secrets.token_bytes(32)).decode() + '\n')
PY
chmod 755 certs results
chmod 644 certs/ca.crt certs/server.crt
printf '%s\n' 'Run: docker compose --env-file tests/soak/.env -f tests/soak/compose.yaml up --build --abort-on-container-exit --exit-code-from runner'
