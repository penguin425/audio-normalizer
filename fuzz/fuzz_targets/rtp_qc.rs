#![no_main]

use forge_normalizer::rtp_qc::{self, RtpAudioProfile};
use libfuzzer_sys::fuzz_target;
use std::fs;

const SDP: &str = "v=0\r\n\
o=- 1 1 IN IP4 192.0.2.1\r\n\
s=Fuzz RTP\r\n\
c=IN IP4 239.1.2.3/32\r\n\
t=0 0\r\n\
m=audio 5004 RTP/AVP 96\r\n\
a=rtpmap:96 L24/48000/2\r\n\
a=ptime:1\r\n\
a=ts-refclk:ptp=IEEE1588-2008:00-11-22-FF-FE-33-44-55:0\r\n\
a=mediaclk:direct=0\r\n";

const SECONDARY_SDP: &str = "v=0\r\n\
o=- 1 1 IN IP4 192.0.2.2\r\n\
s=Fuzz redundant RTP\r\n\
c=IN IP4 239.1.2.4/32\r\n\
t=0 0\r\n\
m=audio 5004 RTP/AVP 96\r\n\
a=rtpmap:96 L24/48000/2\r\n\
a=ptime:1\r\n\
a=ts-refclk:ptp=IEEE1588-2008:00-11-22-FF-FE-33-44-55:0\r\n\
a=mediaclk:direct=0\r\n";

fuzz_target!(|data: &[u8]| {
    if let Ok(directory) = tempfile::tempdir() {
        let sdp = directory.path().join("stream.sdp");
        let capture = directory.path().join("capture.pcap");
        let secondary_sdp = directory.path().join("secondary.sdp");
        let secondary_capture = directory.path().join("secondary.pcap");
        if fs::write(&sdp, SDP).is_ok()
            && fs::write(&capture, data).is_ok()
            && fs::write(&secondary_sdp, SECONDARY_SDP).is_ok()
            && fs::write(&secondary_capture, data).is_ok()
        {
            let _ = rtp_qc::audit(&sdp, Some(&capture), RtpAudioProfile::Smpte2110_30);
            let _ = rtp_qc::audit_st2022_7(
                &sdp,
                &capture,
                &secondary_sdp,
                &secondary_capture,
                RtpAudioProfile::Smpte2110_30,
                Some(1.0),
            );
        }
    }
});
