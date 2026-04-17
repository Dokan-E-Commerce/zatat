#![no_main]

use libfuzzer_sys::fuzz_target;
use zatat_protocol::envelope::parse_inbound;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let _ = parse_inbound(s);
});
