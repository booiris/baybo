# aura-security Fuzz Targets

Fuzz testing for the `aura-security` crate using
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer).

Rule sets and fuzz-harness layout are adapted from NEAR AI's ironclaw
project: <https://github.com/nearai/ironclaw/tree/staging/crates/ironclaw_safety/fuzz>.

## Targets

| Target | What it exercises |
|--------|-------------------|
| `fuzz_injection_detector` | Prompt-injection marker detection (Aho-Corasick + regex), byte-offset invariants |
| `fuzz_leak_detector`      | Secret leak scanning with the default rule set, block/match bookkeeping |
| `fuzz_sensitive_paths`    | `is_sensitive_path` against arbitrary UTF-8 path strings |
| `fuzz_crypto_roundtrip`   | AES-256-GCM `encrypt` / `decrypt` roundtrip and malformed-input robustness |
| `fuzz_placeholder`        | Deterministic HKDF+HMAC placeholder minting and its regex |

## Setup

```bash
cargo install cargo-fuzz
rustup install nightly
```

## Running

```bash
cd crates/security

# Run a specific target until a crash or Ctrl-C.
cargo +nightly fuzz run fuzz_injection_detector

# Time-bounded run.
cargo +nightly fuzz run fuzz_leak_detector -- -max_total_time=300

# Iterate every target for 60s each.
for t in fuzz_injection_detector fuzz_leak_detector fuzz_sensitive_paths \
        fuzz_crypto_roundtrip fuzz_placeholder; do
    echo "==> $t"
    cargo +nightly fuzz run "$t" -- -max_total_time=60
done
```

## Seed corpus

Each target ships a small seed corpus under `corpus/<target>/` with
representative inputs covering the major pattern families the target
handles. The fuzzer uses these as starting points for mutation.
