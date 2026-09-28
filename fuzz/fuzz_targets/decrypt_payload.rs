#![no_main]

use libfuzzer_sys::fuzz_target;
use zatat_protocol::encryption::decrypt_payload;

// The first 32 bytes are the key; the rest is the envelope under test.
fuzz_target!(|data: &[u8]| {
    let Some((key, envelope)) = data.split_first_chunk::<32>() else {
        return;
    };
    let Ok(s) = std::str::from_utf8(envelope) else { return };
    let _ = decrypt_payload(s, key);
});
