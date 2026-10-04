#!/bin/sh
# Regenerate the mTLS client-authentication fixtures in this directory.
#
# Throwaway material for tests only: nothing outside the test suites loads
# it, and every private key here is public on purpose. Run from anywhere:
#
#     sh lib/assets/mtls/generate.sh
#
# Requires OpenSSL >= 3.0 (`openssl ca -crl_lastupdate/-crl_nextupdate`).
# Every validity window ends in 2125, so the fixtures do not expire under the
# tests; the one CRL that must be expired is pinned in 2020 instead.
set -eu

cd "$(dirname "$0")"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

DAYS=36500
FAR_FUTURE=21250101000000Z

# A CA whose KeyUsage carries `cRLSign`: webpki refuses a CRL signed by an
# issuer whose KeyUsage lacks it.
new_ca() { # <name> <common name>
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
        -keyout "$1-key.pem" -out "$1-cert.pem" -days "$DAYS" -subj "/CN=$2" \
        -addext "basicConstraints=critical,CA:TRUE" \
        -addext "keyUsage=critical,keyCertSign,cRLSign" \
        -addext "subjectKeyIdentifier=hash" 2>/dev/null
}

# An `openssl ca` database for one CA, so it can revoke and sign CRLs.
ca_config() { # <name>
    mkdir -p "$work/$1"
    : >"$work/$1/index.txt"
    echo 1000 >"$work/$1/crlnumber"
    cat >"$work/$1/ca.cnf" <<EOF
[ca]
default_ca = CA_default
[CA_default]
database = $work/$1/index.txt
crlnumber = $work/$1/crlnumber
default_md = sha256
crl_extensions = crl_ext
[crl_ext]
authorityKeyIdentifier = keyid:always
EOF
}

gencrl() { # <ca name> <out> <lastUpdate> <nextUpdate>
    openssl ca -batch -config "$work/$1/ca.cnf" -keyfile "$1-key.pem" -cert "$1-cert.pem" \
        -gencrl -crl_lastupdate "$3" -crl_nextupdate "$4" -out "$2" 2>/dev/null
}

new_ca ca sozu-test-mtls-ca
new_ca other-ca sozu-test-mtls-other-ca
ca_config ca
ca_config other-ca

# The client identity: a leaf for TLS client authentication, issued by `ca`.
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout client-key.pem -out "$work/client.csr" -subj "/CN=sozu-test-mtls-client" 2>/dev/null
cat >"$work/client.ext" <<EOF
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = clientAuth
subjectAltName = DNS:sozu-test-mtls-client
authorityKeyIdentifier = keyid
EOF
openssl x509 -req -in "$work/client.csr" -CA ca-cert.pem -CAkey ca-key.pem \
    -set_serial 0x1001 -days "$DAYS" -extfile "$work/client.ext" -out client-cert.pem 2>/dev/null

NOW=$(date -u +%Y%m%d%H%M%SZ)

# Current, revokes nothing: the client must still be accepted.
gencrl ca crl-current.pem "$NOW" "$FAR_FUTURE"
# Same issuer, long past its nextUpdate, revokes nothing: only the expiry
# can reject the client.
gencrl ca crl-expired.pem 20200101000000Z 20200102000000Z
# Another CA's current CRL: it covers nothing in the client's chain, so the
# client's revocation status is unknown.
gencrl other-ca crl-other-ca.pem "$NOW" "$FAR_FUTURE"
# Current, revokes the client. Generated last: revoking updates the database.
openssl ca -batch -config "$work/ca/ca.cnf" -keyfile ca-key.pem -cert ca-cert.pem \
    -revoke client-cert.pem 2>/dev/null
gencrl ca crl-revoked.pem "$NOW" "$FAR_FUTURE"

# The other CA's key signs nothing the tests read back.
rm -f other-ca-key.pem
