# `lib/assets/` — test fixtures

Static fixtures (certificates, sample HTTP bodies) consumed by unit and
integration tests. Production builds do NOT embed or rely on any file
in this directory.

## Certificates

| File                   | CN / SANs                                                      | Purpose                                  |
|------------------------|----------------------------------------------------------------|------------------------------------------|
| `certificate.pem`      | legacy sample                                                  | historical; avoid in new tests           |
| `certificate_chain.pem`| legacy sample chain                                            | historical; avoid in new tests           |
| `cert_test.pem`        | legacy sample                                                  | historical; avoid in new tests           |
| `local-certificate.pem`| CN=`localhost`, SAN=`localhost`                                | single-SNI happy-path tests              |
| `multi-sni-cert.pem`   | CN=`foo.example.com`, SANs=`localhost`, `foo.example.com`, `bar.example.com`, `baz.example.com` | SNI routing / tenant-isolation E2E tests |

### Regenerating `multi-sni-cert.pem` / `multi-sni-key.pem`

Self-signed, 10-year validity, covers the SANs used by the SNI-focused
E2E recipes (FIX-7 through FIX-10):

```bash
cd lib/assets/
openssl req -x509 -nodes -newkey rsa:2048 \
    -keyout multi-sni-key.pem -out multi-sni-cert.pem -days 3650 \
    -subj '/CN=foo.example.com' \
    -addext 'subjectAltName=DNS:localhost,DNS:foo.example.com,DNS:bar.example.com,DNS:baz.example.com'
```

The matching private key is `multi-sni-key.pem`. Both files are
checked in; rotate them together if the cert expires.

## mTLS client authentication (`mtls/`)

Throwaway CA, client identity and CRLs for client certificate
authentication and revocation, consumed by the `client_revocation_*` unit
tests in `lib/src/https.rs`, the `client_auth_*` tests in
`command/src/config.rs` and the `test_mtls_*` e2e tests in
`e2e/src/tests/tls_tests.rs`. Every private key here is public on purpose:
nothing outside the tests trusts this CA.

| File                   | What it is                                                         |
|------------------------|--------------------------------------------------------------------|
| `ca-cert.pem` / `ca-key.pem` | CA `CN=sozu-test-mtls-ca`, KeyUsage `keyCertSign, cRLSign`   |
| `client-cert.pem` / `client-key.pem` | client `CN=sozu-test-mtls-client`, serial `0x1001`, EKU `clientAuth`, issued by the CA |
| `crl-current.pem`      | issued by the CA, `nextUpdate` 2125, revokes nothing               |
| `crl-revoked.pem`      | issued by the CA, `nextUpdate` 2125, revokes the client            |
| `crl-expired.pem`      | issued by the CA, `nextUpdate` 2020-01-02, revokes nothing; refused when a listener is built |
| `crl-partition-a.pem` / `crl-partition-b.pem` | issued by the CA, `nextUpdate` 2125, distinct `IssuingDistributionPoint`, revoke nothing |
| `crl-other-ca.pem`     | issued by `CN=sozu-test-mtls-other-ca`, covers nothing in the client's chain |
| `other-ca-cert.pem`    | the CA that issued `crl-other-ca.pem` (its key is not kept)        |

Each CRL leaves exactly one reason to accept or reject the client: the
revoking one is current, so only the serial lookup rejects; the expired one
is from the right issuer and revokes nothing, so only the expiry rejects.
The handshake-time expiry of a CRL is tested on `crl-current.pem` at a date
past its `nextUpdate` but before the client certificate's `notAfter` (2126),
so keep the CRLs' `nextUpdate` before the certificates' expiry. Keep it that
way when changing them.

Regenerate everything together (OpenSSL >= 3.0):

```bash
sh lib/assets/mtls/generate.sh
```
