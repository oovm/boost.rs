# PostgreSQL TLS fixtures (test-only)

This directory holds throwaway TLS material for the protocol-v3 gateway tests:

| file | purpose |
|---|---|
| `ca.pem` | local CA the test client trusts |
| `server.pem` | leaf certificate for `localhost` and `127.0.0.1` |
| `server.key` | PKCS#8 private key of `server.pem` |

Nothing here is a secret or fits production use. The material is checked in so
the encrypted-session test needs no OpenSSL install and no per-run key
generation.

Regenerate with PowerShell 7+ (`.NET` `CertificateRequest`):

```powershell
pwsh -File generate.ps1
```

The script creates a self-signed CA (`CN=boost-pg-tls-test-ca`) and a leaf
(`CN=localhost`, SAN `DNS:localhost` + `IP:127.0.0.1`, EKU `serverAuth`) signed
by it, valid 2020-01-01 through 2045-01-01.
