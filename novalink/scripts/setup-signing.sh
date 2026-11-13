#!/bin/zsh
# Creates a stable, local-only code-signing identity for NovaLink (idempotent).
#
# Why: ad-hoc signatures change on every build, so macOS re-asks for Keychain access each time.
# A self-signed certificate gives a designated requirement pinned to that certificate, so one
# "Always Allow" lasts across rebuilds. The certificate is NOT added to any trust settings and the
# private key lives in its own keychain file (not your login keychain). Its random password is
# stored in your login keychain under "com.novalink.signing".
set -euo pipefail

IDENTITY="NovaLink Local Signing"
DIR="$HOME/Library/Application Support/NovaLink/signing"
KC="$DIR/signing.keychain-db"
PW_SERVICE="com.novalink.signing"

kc_password() { security find-generic-password -s "$PW_SERVICE" -a keychain -w 2>/dev/null; }

if [[ -f "$KC" ]] && PW="$(kc_password)" && security unlock-keychain -p "$PW" "$KC" 2>/dev/null \
   && security find-identity -p codesigning "$KC" | grep -q "\"$IDENTITY\""; then
  exit 0
fi

echo "==> Creating local signing identity \"$IDENTITY\""
mkdir -p "$DIR"; chmod 700 "$DIR"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
PW="$(openssl rand -hex 24)"
P12PW="$(openssl rand -hex 24)"

cat > "$TMP/cert.cnf" <<CNF
[req]
distinguished_name=dn
x509_extensions=v3
prompt=no
[dn]
CN=$IDENTITY
[v3]
basicConstraints=critical,CA:false
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
CNF
openssl req -x509 -newkey rsa:3072 -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -days 3650 -nodes -config "$TMP/cert.cnf" 2>/dev/null
# macOS only imports PKCS#12 with legacy (SHA1/3DES) encryption.
openssl pkcs12 -export -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -out "$TMP/id.p12" -passout "pass:$P12PW" \
  -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1

rm -f "$KC"
security create-keychain -p "$PW" "$KC"
security set-keychain-settings "$KC"                # no auto-lock timeout
security unlock-keychain -p "$PW" "$KC"
security import "$TMP/id.p12" -k "$KC" -P "$P12PW" -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k "$PW" "$KC" >/dev/null
security add-generic-password -U -s "$PW_SERVICE" -a keychain -l "NovaLink signing keychain" -w "$PW"
echo "    Identity stored in $KC (valid 10 years)"
