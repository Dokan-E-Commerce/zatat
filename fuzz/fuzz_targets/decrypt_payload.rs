#![no_main]

use libfuzzer_sys::fuzz_target;
use zatat_protocol::encryption::decrypt_payload;

// The 32-byte key is fixed — we're testing the envelope parser +
// secretbox decrypt glue, not key material.
fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let key = [0x42u8; 32];
    let _ = decrypt_payload(s, &key);
});
