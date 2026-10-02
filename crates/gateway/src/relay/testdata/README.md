Throwaway TLS fixtures for `relay/dial.rs` tests only — never trusted outside them.

- `test-ca.pem` — self-signed test CA (the CA key was discarded after signing).
- `relay-test-cert.pem` / `relay-test-key.pem` — leaf for `DNS:relay.test`, signed by the CA.
