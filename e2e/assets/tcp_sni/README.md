# `e2e/assets/tcp_sni/` — TLS-terminating mock-backend fixtures

Static, throwaway TLS certificates consumed ONLY by
`e2e/src/tests/tcp_sni_tests.rs` and `e2e/src/mock/tls_backend.rs` (the
TCP passthrough SNI+ALPN preread e2e coverage for sozu-proxy/sozu#1279).
Production builds do NOT embed or rely on any file in this directory —
Sōzu never terminates TLS on the TCP passthrough path; these certs
belong to the MOCK BACKENDS the tests spin up, never to `sozu` itself.

All certificates are self-signed off one throwaway 10-year CA generated
for this test suite only (`ca-key.pem` is intentionally NOT checked in —
nothing at runtime loads it, only `ca-cert.pem` is needed as the trust
anchor for the mTLS test's `WebPkiClientVerifier`).

| File | CN / SAN | Purpose |
|---|---|---|
| `ca-cert.pem` | CN=`sozu-e2e-tcp-sni-test-ca` | Root of trust for the mTLS backend's client-cert verifier |
| `backend-a-cert.pem` / `-key.pem` | SAN=`a.example.com` | V7 SNI-routing backend A |
| `backend-b-cert.pem` / `-key.pem` | SAN=`b.example.com` | V7 SNI-routing backend B |
| `mtls-backend-cert.pem` / `-key.pem` | SAN=`mtls.example.com` | V7 mTLS backend's own server cert |
| `mtls-client-cert.pem` / `-key.pem` | SAN=`e2e-mtls-client` | V7 mTLS test's in-test client certificate, signed by the same CA so the backend's `WebPkiClientVerifier` accepts it |

The client-side `rustls::ClientConfig` used against these backends
always installs the permissive `mock::https_client::Verifier` (already
used by `tls_tests.rs`), so none of these leaf certs need to chain to a
system trust store — only the mTLS backend's OWN verification of the
CLIENT certificate against `ca-cert.pem` is a real signature check.

## Regenerating

```bash
cd /tmp && mkdir tcp_sni_certs && cd tcp_sni_certs

# Throwaway CA
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 3650 \
  -keyout ca-key.pem -out ca-cert.pem \
  -subj "/O=sozu-e2e-test/CN=sozu-e2e-tcp-sni-test-ca" \
  -addext "basicConstraints=critical,CA:true" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

# One leaf per row above, e.g. backend-a:
openssl req -new -newkey rsa:2048 -nodes \
  -keyout backend-a-key.pem -out backend-a.csr -subj "/CN=a.example.com"
openssl x509 -req -in backend-a.csr -CA ca-cert.pem -CAkey ca-key.pem -CAcreateserial \
  -days 3650 -sha256 -out backend-a-cert.pem \
  -extfile <(printf "subjectAltName=DNS:a.example.com\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:false")
# repeat for backend-b (DNS:b.example.com), mtls-backend (DNS:mtls.example.com,
# extendedKeyUsage=serverAuth), and mtls-client (CN=e2e-mtls-client,
# subjectAltName=DNS:e2e-mtls-client, extendedKeyUsage=clientAuth).
```

Copy the resulting `*-cert.pem` / `*-key.pem` pairs (and `ca-cert.pem`,
but NOT `ca-key.pem`) into this directory. Rotate all files together if
a cert expires (10-year validity from 2026-07-09).
