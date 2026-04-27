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
