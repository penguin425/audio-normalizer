#![no_main]

use forge_normalizer::ebu_qc_validation::{validate_xml, EbuQcValidationProfile};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = validate_xml(data, EbuQcValidationProfile::DataModel2026_04);
    let _ = validate_xml(data, EbuQcValidationProfile::Scenario1);
});
