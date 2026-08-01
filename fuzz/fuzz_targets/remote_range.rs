#![no_main]

use forge_normalizer::remote_range::{parse_content_range, RemoteObjectUri};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 16 * 1024 {
        return;
    }
    let value = String::from_utf8_lossy(data);
    let _ = RemoteObjectUri::parse(&value, false);
    let _ = parse_content_range(&value);
});
