#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use zatat_protocol::http_sign::{sign_http, verify_http};

// Two invariants: verify never panics on garbage, and sign→verify
// always round-trips for any input we can construct.
#[derive(Debug, Arbitrary)]
struct Input {
    method: String,
    path: String,
    body: Vec<u8>,
    pairs: Vec<(String, String)>,
    given_signature: String,
    secret: String,
}

fuzz_target!(|input: Input| {
    let _ = verify_http(
        &input.method,
        &input.path,
        &input.body,
        &input.pairs,
        &input.given_signature,
        &input.secret,
    );

    let sig = sign_http(
        &input.method,
        &input.path,
        &input.body,
        &input.pairs,
        &input.secret,
    );
    assert!(verify_http(
        &input.method,
        &input.path,
        &input.body,
        &input.pairs,
        &sig,
        &input.secret,
    ));
});
