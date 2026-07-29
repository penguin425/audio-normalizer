#![no_main]

use forge_normalizer::dash_qc::{self, DashProfile};
use libfuzzer_sys::fuzz_target;
use std::io::Write;

const PATCH_BASE: &[u8] = br#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" id="live"
 type="dynamic" availabilityStartTime="2026-07-29T00:00:00Z"
 publishTime="2026-07-29T00:00:00Z" minimumUpdatePeriod="PT2S"
 minBufferTime="PT1S"><!--seed--><?audit seed?><Label>seed</Label>
 <Period id="p"/></MPD>"#;

fuzz_target!(|data: &[u8]| {
    if let Ok(mut file) = tempfile::NamedTempFile::new() {
        if file.write_all(data).is_ok() {
            let _ = dash_qc::audit(file.path(), DashProfile::Iso23009);
            let _ = dash_qc::audit(file.path(), DashProfile::DashIfIop);
            let _ = dash_qc::audit(file.path(), DashProfile::DashLive);
            let _ = dash_qc::observation_targets(file.path());
            let _ = dash_qc::audit_with_previous(
                file.path(),
                file.path(),
                DashProfile::Iso23009,
            );
        }
    }
    const SEPARATOR: &[u8] = b"\n--FORGE-MPD-PATCH--\n";
    if let Some(split) = data
        .windows(SEPARATOR.len())
        .position(|window| window == SEPARATOR)
    {
        let patch_start = split + SEPARATOR.len();
        if let (Ok(mut base), Ok(mut patch)) = (
            tempfile::NamedTempFile::new(),
            tempfile::NamedTempFile::new(),
        ) {
            if base.write_all(&data[..split]).is_ok()
                && patch.write_all(&data[patch_start..]).is_ok()
            {
                let _ =
                    dash_qc::audit_with_patch(base.path(), patch.path(), DashProfile::Iso23009);
                let _ = dash_qc::observation_targets_with_patch(base.path(), patch.path());
            }
        }
    }

    let selector = data
        .iter()
        .map(|byte| match byte {
            b'&' => "&amp;".to_owned(),
            b'"' => "&quot;".to_owned(),
            b'<' => "&lt;".to_owned(),
            byte if byte.is_ascii_graphic() || *byte == b' ' => char::from(*byte).to_string(),
            _ => "x".to_owned(),
        })
        .collect::<String>();
    let synthetic_patch = format!(
        r#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020" mpdId="live"
 originalPublishTime="2026-07-29T00:00:00Z" publishTime="2026-07-29T00:00:02Z">
 <remove sel="{selector}"/></Patch>"#
    );
    if let (Ok(mut base), Ok(mut patch)) = (
        tempfile::NamedTempFile::new(),
        tempfile::NamedTempFile::new(),
    ) {
        if base.write_all(PATCH_BASE).is_ok()
            && patch.write_all(synthetic_patch.as_bytes()).is_ok()
        {
            let _ = dash_qc::audit_with_patch(base.path(), patch.path(), DashProfile::Iso23009);
        }
    }

    let node_patch = format!(
        r#"<Patch xmlns="urn:mpeg:dash:schema:mpd-patch:2020"
 xmlns:m="urn:mpeg:dash:schema:mpd:2011" mpdId="live"
 originalPublishTime="2026-07-29T00:00:00Z" publishTime="2026-07-29T00:00:02Z">
 <replace sel="/m:MPD/@publishTime">2026-07-29T00:00:02Z</replace>
 <replace sel="/MPD/comment()[1]"><!--updated--></replace>
 <replace sel="/MPD/processing-instruction('audit')"><?audit updated?></replace>
 <replace sel="/MPD/Label/text()[1]">{selector}</replace>
 <add sel="/MPD" type="namespace::temporary">urn:forge:fuzz</add>
 <replace sel="/MPD/namespace::temporary">urn:forge:fuzz:updated</replace>
 <remove sel="/MPD/namespace::temporary"/>
 </Patch>"#
    );
    if let (Ok(mut base), Ok(mut patch)) = (
        tempfile::NamedTempFile::new(),
        tempfile::NamedTempFile::new(),
    ) {
        if base.write_all(PATCH_BASE).is_ok()
            && patch.write_all(node_patch.as_bytes()).is_ok()
        {
            let _ = dash_qc::audit_with_patch(base.path(), patch.path(), DashProfile::Iso23009);
        }
    }
});
