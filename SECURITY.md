# Security policy

## Reporting a vulnerability

Please report security issues **privately**. Do not open a public GitHub
issue for a suspected vulnerability.

- **GitHub Security Advisories** (preferred):
  <https://github.com/codetestcode/tak-rs/security/advisories/new>
- **Email:** dj@codetestcode.io
- **Machine-readable contact:** [.well-known/security.txt](assets/site/.well-known/security.txt)
  (RFC 9116)

We will acknowledge receipt within 72 hours and aim to provide an initial
assessment within seven days. If a fix is needed, we coordinate disclosure
with the reporter before publishing.

## Scope

In scope:

- The crates published under this repository (`tak-cot`, `tak-proto`,
  `tak-net`, `tak-bus`, `tak-store`, `tak-mission`, `tak-config`,
  `tak-server`, `tak-plugin-host`, `taktool`, and the verification
  harnesses).
- Anything that lands on the wire: framing, mTLS configuration,
  authentication, group/role enforcement, subscription filtering.
- Persistence and the Mission API surface.

Out of scope:

- The marketing site (`assets/site/`).
- The upstream Java TAK Server (report to the TAK Product Center).
- Vulnerabilities in third-party dependencies — please report those
  upstream first; we will pick up the fix on release.

## Supported versions

`tak-rs` is pre-1.0. Only `main` is supported. Once we cut a 1.0,
this section will pin the supported branches.

## Hardening posture

- TLS is `rustls` only — `openssl-sys` and `native-tls` are banned via
  `deny.toml`.
- No `unsafe` blocks ship without an `unsafe-auditor` review.
- Dependency advisories are gated by `cargo deny` in the pre-push hook.
- Codec inputs are continuously fuzzed (`cargo-fuzz` on the XML decoder
  and the streaming framer) with sanitizers under nightly.

## Acknowledgments

Coordinated reports are credited at
<https://github.com/codetestcode/tak-rs/security/advisories>
unless the reporter requests otherwise.
