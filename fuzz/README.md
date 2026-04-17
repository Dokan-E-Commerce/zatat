# Fuzzing

`cargo-fuzz` harnesses for the security-sensitive parsers and crypto
paths in `zatat-protocol`.

## What's fuzzed

| Target                 | Covers                                                        |
|------------------------|---------------------------------------------------------------|
| `parse_inbound`        | Client → server JSON envelope parser                          |
| `verify_http`          | HTTP API HMAC signature verification + sign/verify round-trip |
| `decrypt_payload`      | NaCl Secretbox envelope parser for `private-encrypted-*`      |
| `presence_from_members`| `PresenceData::from_members` + subscription-data serializer   |

Each harness asserts the same invariant the production code relies on —
these functions must never panic on arbitrary input.

## Requirements

- Rust **nightly** (auto-selected by `rust-toolchain.toml` in this dir)
- Linux or macOS. `libfuzzer-sys` doesn't work on Windows.
- `cargo install cargo-fuzz`

## Run locally

From this directory (`server/fuzz/`):

```sh
# 60 seconds each, all four targets
for t in parse_inbound verify_http decrypt_payload presence_from_members; do
  cargo fuzz run "$t" --release -- -max_total_time=60
done
```

Or run a single target indefinitely:

```sh
cargo fuzz run verify_http --release
```

Press Ctrl-C to stop. Interesting inputs (crashes, hangs, OOMs) are
written to `artifacts/<target>/`. Shrink a failing input to the minimum
reproducer:

```sh
cargo fuzz tmin verify_http artifacts/verify_http/crash-<hash>
```

Reproduce a crash deterministically:

```sh
cargo fuzz run verify_http artifacts/verify_http/crash-<hash>
```

## CI

The `fuzz-smoke` job in `.github/workflows/ci.yml` runs each target for
30 s on every push and PR. Any new panic blocks the merge. Crash
artifacts are uploaded from the failing run.

## Adding a new target

1. Drop a new `fuzz_targets/<name>.rs` with a `fuzz_target!` macro.
2. Add a matching `[[bin]]` section to `Cargo.toml`.
3. Add the name to the loop in `.github/workflows/ci.yml`.
