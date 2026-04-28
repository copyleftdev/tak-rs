# 0002 — TLS cipher-suite compatibility: rustls 0.23 + aws_lc_rs + tls12

- **Date:** 2026-04-28
- **Status:** Accepted
- **Issue:** [#1](https://github.com/copyleftdev/tak-rs/issues/1)
- **Open question resolved:** `docs/architecture.md` §9 #1
- **Unblocks:** M1 (TLS listener, issues #16-#20)

## Context

`tak-net` will accept mTLS streaming connections on port 8089 — the
production transport for ATAK / iTAK / WinTAK. We must confirm that
rustls 0.23 with the `aws_lc_rs` cryptography provider + `tls12` feature
flag negotiates the cipher suites the upstream Java server has historically
offered, so clients in the field connect without falling back to a
broken-handshake error.

The risk we were sizing: if rustls rejects the suites a real ATAK device
offers, every existing deployment's clients would silently fail to connect
when migrated.

## Evidence

### Java server cipher allow-list (definitive)

Recon of `.scratch/takserver-java/src/takserver-core/src/main/java/com/bbn/marti/service/SSLConfig.java` lines 404–411:

```java
private static String[] getCiphers(SSLContext sslContext) {
    List<String> wantedCiphers = new LinkedList<String>();
    // these are Suite B ciphers from http://tools.ietf.org/html/rfc6460
    // always add the 256-bit one:
    wantedCiphers.add("TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384");
    wantedCiphers.add("TLS_AES_128_GCM_SHA256");
    wantedCiphers.add("TLS_AES_256_GCM_SHA384");

    String[] supportedCipherArray = sslContext != null
        ? sslContext.getSupportedSSLParameters().getCipherSuites()
        : new String[]{};
    Set<String> supportedCiphers = new HashSet<String>(Arrays.asList(supportedCipherArray));
    wantedCiphers.retainAll(supportedCiphers);
    return wantedCiphers.toArray(new String[]{});
}
```

The server offers **exactly three** cipher suites, taken from RFC 6460
("NSA Suite B Cryptographic Suites for TLS"):

| Java name | TLS version | Rustls equivalent |
|---|---|---|
| `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384` | TLS 1.2 | `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384` |
| `TLS_AES_128_GCM_SHA256`                   | TLS 1.3 | `TLS13_AES_128_GCM_SHA256`                 |
| `TLS_AES_256_GCM_SHA384`                   | TLS 1.3 | `TLS13_AES_256_GCM_SHA384`                 |

TLS protocol context comes from `CoreConfig.example.xml`'s `<tls
context="TLSv1.2">` element, which `SSLConfig.getProtocols()` returns
verbatim. Newer TAK Server deployments may set `TLSv1.2,TLSv1.3` (see
`tak.server.ServerConfiguration` line 548); the example default is just
`TLSv1.2`.

Federation (`tak.server.federation.SSLConfig.java:67`) uses
`TLSv1.2,TLSv1.3` with cipher suites configurable via CoreConfig — out of
scope here (federation is deferred per architecture §1).

### rustls 0.23 + aws_lc_rs cipher support

Verified against rustls 0.23.39 with the `aws_lc_rs` provider and the
`tls12` feature flag (the configuration set in our workspace `Cargo.toml`).

**TLS 1.3 (default; always offered):**

- `TLS13_AES_128_GCM_SHA256` — exact match
- `TLS13_AES_256_GCM_SHA384` — exact match
- `TLS13_CHACHA20_POLY1305_SHA256` — extra (we exclude in our config)

**TLS 1.2 (requires `tls12` feature):**

- `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384` — exact match
- `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256` — extra
- `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256` — extra
- `TLS_ECDHE_RSA_*` × 3 — extra (RSA cert chains; not in Suite B)

**Conclusion: rustls 0.23 + aws_lc_rs + tls12 supports a strict superset
of every cipher suite the Java server has historically offered.** No
client that successfully connected to the Java server can fail to
connect to a tak-rs server configured with the same allow-list.

## Decision

`tak-net` configures a `rustls::ServerConfig` with:

```rust
use std::sync::Arc;
use rustls::crypto::CryptoProvider;
use rustls::crypto::aws_lc_rs;
use rustls::version::{TLS12, TLS13};

fn approved_cipher_suites() -> Vec<rustls::SupportedCipherSuite> {
    vec![
        // TLS 1.3 — preferred when client supports it.
        aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384,
        aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256,
        // TLS 1.2 fallback for older ATAK clients that haven't moved to 1.3.
        aws_lc_rs::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    ]
}

pub fn server_provider() -> Arc<CryptoProvider> {
    Arc::new(CryptoProvider {
        cipher_suites: approved_cipher_suites(),
        ..aws_lc_rs::default_provider()
    })
}

pub fn server_config(/* certs, key, client_verifier */) -> Result<rustls::ServerConfig> {
    rustls::ServerConfig::builder_with_provider(server_provider())
        .with_protocol_versions(&[&TLS13, &TLS12])
        .with_client_cert_verifier(/* ... */)
        .with_single_cert(/* ... */)
}
```

This pins us to the exact same negotiation surface the Java server
exposes. ChaCha20-Poly1305 and AES-128-GCM-on-TLS-1.2 are deliberately
omitted to keep the surface small (Suite B was always the intent).

## Why we didn't need a real ATAK pcap to decide

The cipher list isn't probabilistic — it's enumerated in the server's
source code on lines we read directly. ATAK clients can't negotiate
suites the server doesn't offer, so the question reduces to:

1. *Is rustls's offering a superset of the Java server's offering?* —
   Yes, established above.
2. *Will a TLS handshake succeed?* — Yes, because every suite the Java
   server accepts is one rustls also accepts; clients in the field have
   already proven they offer at least one of these three.

A pcap **confirms** but doesn't add information beyond cipher choice.
What a pcap *would* let us validate is everything around the handshake:
SNI behavior, session-resumption tickets, ALPN extensions if any,
extended-master-secret, and the cert-chain offering of mismatched ATAK
versions. Those are M1-implementation concerns rather than this
decision's scope.

## Action: capture method for the future

When an ATAK device is available, run:

```sh
sudo scripts/capture-atak-handshake.sh 8089 /tmp/atak.pcap
```

(commits in this same change). The script tcpdumps port 8089 to a pcap;
analysis with:

```sh
tshark -r /tmp/atak.pcap -Y 'tls.handshake.type==1' -V \
    | grep -E 'Cipher Suite:|Version:|Extension'
```

confirms cipher suites + TLS extensions a real client offers. If the
ClientHello suite list contains any of our three approved suites, the
handshake will succeed.

## Consequences

- **M1 unblocked.** Issue #16 (rustls server config builder) can implement
  the approved-cipher list verbatim. No further design uncertainty on TLS.
- **Cipher list is part of the public API surface.** Adding or removing a
  cipher requires another ADR — not a casual change.
- **Federation is out of scope here.** When we bring up gRPC federation
  (deferred), revisit because the federation Java code allows configurable
  ciphers — we may need a wider allow-list there.
- **No openssl-sys, no native-tls.** Invariant D4 stands as written; this
  ADR confirms there's no operational reason to weaken it.

## Reproducing the recon

```sh
git -C .scratch/takserver-java rev-parse HEAD       # 5187abd...
grep -B2 -A12 'getCiphers' \
  .scratch/takserver-java/src/takserver-core/src/main/java/com/bbn/marti/service/SSLConfig.java
```
