//! Cross-platform CLAP wrapper for Forge's callback-safe stereo processor.

use crate::realtime::{RealtimeGainConfig, RealtimeGainProcessor};
use clack_extensions::audio_ports::*;
use clack_extensions::latency::{PluginLatency, PluginLatencyImpl};
use clack_extensions::params::*;
use clack_extensions::render::{PluginRender, PluginRenderImpl, RenderMode};
use clack_extensions::state::{PluginState, PluginStateImpl};
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};
use std::ffi::CStr;
use std::fmt::Write as _;
use std::io::{Read, Write as _};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

const PARAM_GAIN: ClapId = ClapId::new(1);
const PARAM_CEILING: ClapId = ClapId::new(2);
const PARAM_ATTACK: ClapId = ClapId::new(3);
const PARAM_RELEASE: ClapId = ClapId::new(4);
const PARAM_BYPASS: ClapId = ClapId::new(5);
const STATE_MAGIC: &[u8; 12] = b"FORGECLAP1\0\0";

const DEFAULT_GAIN_DB: f32 = 0.0;
const DEFAULT_CEILING_DBTP: f32 = -1.0;
const DEFAULT_ATTACK_MS: f32 = 10.0;
const DEFAULT_RELEASE_MS: f32 = 100.0;

pub struct ForgeClapPlugin;

impl Plugin for ForgeClapPlugin {
    type AudioProcessor<'a> = ForgeClapAudioProcessor<'a>;
    type Shared<'a> = ForgeClapShared;
    type MainThread<'a> = ForgeClapMainThread<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _shared: Option<&ForgeClapShared>) {
        builder
            .register::<PluginAudioPorts>()
            .register::<PluginParams>()
            .register::<PluginState>()
            .register::<PluginLatency>()
            .register::<PluginRender>();
    }
}

impl DefaultPluginFactory for ForgeClapPlugin {
    fn get_descriptor() -> PluginDescriptor {
        use clack_plugin::plugin::features::*;
        PluginDescriptor::new(
            "io.github.penguin425.audio-normalizer.forge-live",
            "Forge Live",
        )
        .with_vendor("penguin425")
        .with_url("https://github.com/penguin425/audio-normalizer")
        .with_version(env!("CARGO_PKG_VERSION"))
        .with_description("Smoothed gain and true-peak limiting for stereo audio")
        .with_features([AUDIO_EFFECT, STEREO])
    }

    fn new_shared(_host: HostSharedHandle<'_>) -> Result<Self::Shared<'_>, PluginError> {
        Ok(ForgeClapShared::new())
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        shared: &'a Self::Shared<'a>,
    ) -> Result<Self::MainThread<'a>, PluginError> {
        Ok(ForgeClapMainThread { shared })
    }
}

pub struct ForgeClapShared {
    params: ForgeClapParams,
    latency_frames: AtomicU32,
    render_mode: AtomicI32,
}

impl ForgeClapShared {
    fn new() -> Self {
        Self {
            params: ForgeClapParams::new(),
            latency_frames: AtomicU32::new(0),
            render_mode: AtomicI32::new(RenderMode::Realtime as i32),
        }
    }
}

impl PluginShared<'_> for ForgeClapShared {}

pub struct ForgeClapMainThread<'a> {
    shared: &'a ForgeClapShared,
}

impl<'a> PluginMainThread<'a, ForgeClapShared> for ForgeClapMainThread<'a> {}

pub struct ForgeClapAudioProcessor<'a> {
    shared: &'a ForgeClapShared,
    processor: RealtimeGainProcessor,
}

impl<'a> PluginAudioProcessor<'a, ForgeClapShared, ForgeClapMainThread<'a>>
    for ForgeClapAudioProcessor<'a>
{
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &mut ForgeClapMainThread,
        shared: &'a ForgeClapShared,
        audio_config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        if !audio_config.sample_rate.is_finite()
            || !(1.0..=u32::MAX as f64).contains(&audio_config.sample_rate)
        {
            return Err(PluginError::Message("Invalid sample rate"));
        }
        let values = shared.params.values();
        let processor = RealtimeGainProcessor::new(
            audio_config.sample_rate.round() as u32,
            2,
            RealtimeGainConfig {
                initial_gain_db: values.gain_db as f64,
                ceiling_dbfs: values.ceiling_dbtp as f64,
                attack_ms: values.attack_ms as f64,
                release_ms: values.release_ms as f64,
            },
        )
        .map_err(|_| PluginError::Message("Invalid Forge processor settings"))?;
        shared.latency_frames.store(
            processor.latency_frames().try_into().unwrap_or(u32::MAX),
            Ordering::Relaxed,
        );
        Ok(Self { shared, processor })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        let mut port_pair = audio
            .port_pair(0)
            .ok_or(PluginError::Message("No stereo input/output port"))?;
        let mut channels = port_pair
            .channels()?
            .into_f32()
            .ok_or(PluginError::Message("Forge Live requires f32 audio"))?;
        if channels.channel_pair_count() != 2 {
            return Err(PluginError::Message("Forge Live requires stereo audio"));
        }
        let mut buffers = [None, None];
        for (pair, buffer) in channels.iter_mut().zip(&mut buffers) {
            *buffer = match pair {
                ChannelPair::InputOnly(_) => None,
                ChannelPair::OutputOnly(output) => {
                    output.fill(0.0);
                    Some(output)
                }
                ChannelPair::InPlace(samples) => Some(samples),
                ChannelPair::InputOutput(input, output) => {
                    output.copy_from_slice(input);
                    Some(output)
                }
            };
        }
        let [left, right] = &mut buffers;
        let (Some(left), Some(right)) = (left.as_deref_mut(), right.as_deref_mut()) else {
            return Ok(ProcessStatus::ContinueIfNotQuiet);
        };

        for batch in events.input.batch() {
            for event in batch.events() {
                self.shared.params.handle_event(event);
            }
            self.apply_parameters()?;
            if self.shared.params.bypass() {
                continue;
            }
            let bounds = batch.sample_bounds();
            let left = &mut left[bounds];
            let right = &mut right[batch.sample_bounds()];
            for (left, right) in left.iter_mut().zip(right) {
                let mut frame = [*left, *right];
                self.processor
                    .process_interleaved(&mut frame)
                    .map_err(|_| PluginError::Message("Forge processing failed"))?;
                *left = frame[0];
                *right = frame[1];
            }
        }
        Ok(ProcessStatus::ContinueIfNotQuiet)
    }
}

impl ForgeClapAudioProcessor<'_> {
    fn apply_parameters(&mut self) -> Result<(), PluginError> {
        let values = self.shared.params.values();
        self.processor
            .set_target_gain_db(values.gain_db as f64)
            .and_then(|_| self.processor.set_ceiling_dbfs(values.ceiling_dbtp as f64))
            .and_then(|_| {
                self.processor
                    .set_smoothing(values.attack_ms as f64, values.release_ms as f64)
            })
            .map_err(|_| PluginError::Message("Invalid Forge parameter event"))
    }
}

impl PluginAudioPortsImpl for ForgeClapMainThread<'_> {
    fn count(&mut self, _is_input: bool) -> u32 {
        1
    }

    fn get(&mut self, index: u32, _is_input: bool, writer: &mut AudioPortInfoWriter) {
        if index == 0 {
            writer.set(&AudioPortInfo {
                id: ClapId::new(0),
                name: b"Main",
                channel_count: 2,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::STEREO),
                in_place_pair: None,
            });
        }
    }
}

impl PluginLatencyImpl for ForgeClapMainThread<'_> {
    fn get(&mut self) -> u32 {
        self.shared.latency_frames.load(Ordering::Relaxed)
    }
}

impl PluginRenderImpl for ForgeClapMainThread<'_> {
    fn has_hard_realtime_requirement(&self) -> bool {
        false
    }

    fn set(&mut self, mode: RenderMode) -> Result<(), PluginError> {
        self.shared
            .render_mode
            .store(mode as i32, Ordering::Relaxed);
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ParamValues {
    gain_db: f32,
    ceiling_dbtp: f32,
    attack_ms: f32,
    release_ms: f32,
    bypass: f32,
}

struct ForgeClapParams {
    gain_db: AtomicF32,
    ceiling_dbtp: AtomicF32,
    attack_ms: AtomicF32,
    release_ms: AtomicF32,
    bypass: AtomicF32,
}

impl ForgeClapParams {
    fn new() -> Self {
        Self {
            gain_db: AtomicF32::new(DEFAULT_GAIN_DB),
            ceiling_dbtp: AtomicF32::new(DEFAULT_CEILING_DBTP),
            attack_ms: AtomicF32::new(DEFAULT_ATTACK_MS),
            release_ms: AtomicF32::new(DEFAULT_RELEASE_MS),
            bypass: AtomicF32::new(0.0),
        }
    }

    fn values(&self) -> ParamValues {
        ParamValues {
            gain_db: self.gain_db.load(),
            ceiling_dbtp: self.ceiling_dbtp.load(),
            attack_ms: self.attack_ms.load(),
            release_ms: self.release_ms.load(),
            bypass: self.bypass.load(),
        }
    }

    fn set_values(&self, values: ParamValues) {
        self.gain_db.store(values.gain_db.clamp(-24.0, 24.0));
        self.ceiling_dbtp
            .store(values.ceiling_dbtp.clamp(-12.0, 0.0));
        self.attack_ms.store(values.attack_ms.clamp(1.0, 200.0));
        self.release_ms
            .store(values.release_ms.clamp(10.0, 2_000.0));
        self.bypass
            .store(if values.bypass >= 0.5 { 1.0 } else { 0.0 });
    }

    fn bypass(&self) -> bool {
        self.bypass.load() >= 0.5
    }

    fn handle_event(&self, event: &UnknownEvent) {
        if let Some(CoreEventSpace::ParamValue(event)) = event.as_core_event() {
            let value = event.value() as f32;
            match event.param_id() {
                Some(PARAM_GAIN) => self.gain_db.store(value.clamp(-24.0, 24.0)),
                Some(PARAM_CEILING) => self.ceiling_dbtp.store(value.clamp(-12.0, 0.0)),
                Some(PARAM_ATTACK) => self.attack_ms.store(value.clamp(1.0, 200.0)),
                Some(PARAM_RELEASE) => self.release_ms.store(value.clamp(10.0, 2_000.0)),
                Some(PARAM_BYPASS) => self.bypass.store(if value >= 0.5 { 1.0 } else { 0.0 }),
                _ => {}
            }
        }
    }

    fn value(&self, id: ClapId) -> Option<f64> {
        match id {
            PARAM_GAIN => Some(self.gain_db.load() as f64),
            PARAM_CEILING => Some(self.ceiling_dbtp.load() as f64),
            PARAM_ATTACK => Some(self.attack_ms.load() as f64),
            PARAM_RELEASE => Some(self.release_ms.load() as f64),
            PARAM_BYPASS => Some(self.bypass.load() as f64),
            _ => None,
        }
    }
}

impl PluginMainThreadParams for ForgeClapMainThread<'_> {
    fn count(&mut self) -> u32 {
        5
    }

    fn get_info(&mut self, index: u32, writer: &mut ParamInfoWriter) {
        let info = match index {
            0 => ParamInfo {
                id: PARAM_GAIN,
                flags: ParamInfoFlags::IS_AUTOMATABLE,
                cookie: Default::default(),
                name: b"Gain",
                module: b"Level",
                min_value: -24.0,
                max_value: 24.0,
                default_value: DEFAULT_GAIN_DB as f64,
            },
            1 => ParamInfo {
                id: PARAM_CEILING,
                flags: ParamInfoFlags::IS_AUTOMATABLE,
                cookie: Default::default(),
                name: b"True Peak Ceiling",
                module: b"Limiter",
                min_value: -12.0,
                max_value: 0.0,
                default_value: DEFAULT_CEILING_DBTP as f64,
            },
            2 => ParamInfo {
                id: PARAM_ATTACK,
                flags: ParamInfoFlags::IS_AUTOMATABLE,
                cookie: Default::default(),
                name: b"Attack",
                module: b"Smoothing",
                min_value: 1.0,
                max_value: 200.0,
                default_value: DEFAULT_ATTACK_MS as f64,
            },
            3 => ParamInfo {
                id: PARAM_RELEASE,
                flags: ParamInfoFlags::IS_AUTOMATABLE,
                cookie: Default::default(),
                name: b"Release",
                module: b"Smoothing",
                min_value: 10.0,
                max_value: 2_000.0,
                default_value: DEFAULT_RELEASE_MS as f64,
            },
            4 => ParamInfo {
                id: PARAM_BYPASS,
                flags: ParamInfoFlags::IS_AUTOMATABLE
                    | ParamInfoFlags::IS_STEPPED
                    | ParamInfoFlags::IS_BYPASS,
                cookie: Default::default(),
                name: b"Bypass",
                module: b"",
                min_value: 0.0,
                max_value: 1.0,
                default_value: 0.0,
            },
            _ => return,
        };
        writer.set(&info);
    }

    fn get_value(&mut self, param_id: ClapId) -> Option<f64> {
        self.shared.params.value(param_id)
    }

    fn value_to_text(
        &mut self,
        param_id: ClapId,
        value: f64,
        writer: &mut ParamDisplayWriter,
    ) -> std::fmt::Result {
        match param_id {
            PARAM_GAIN | PARAM_CEILING => write!(writer, "{value:.2} dB"),
            PARAM_ATTACK | PARAM_RELEASE => write!(writer, "{value:.1} ms"),
            PARAM_BYPASS => writer.write_str(if value >= 0.5 { "On" } else { "Off" }),
            _ => Err(std::fmt::Error),
        }
    }

    fn text_to_value(&mut self, param_id: ClapId, text: &CStr) -> Option<f64> {
        let text = text.to_str().ok()?.trim();
        match param_id {
            PARAM_BYPASS => match text.to_ascii_lowercase().as_str() {
                "on" | "true" | "1" => Some(1.0),
                "off" | "false" | "0" => Some(0.0),
                _ => None,
            },
            PARAM_GAIN | PARAM_CEILING | PARAM_ATTACK | PARAM_RELEASE => {
                text.split_whitespace().next()?.parse().ok()
            }
            _ => None,
        }
    }

    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        for event in input {
            self.shared.params.handle_event(event);
        }
    }
}

impl PluginAudioProcessorParams for ForgeClapAudioProcessor<'_> {
    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        for event in input {
            self.shared.params.handle_event(event);
        }
        let _ = self.apply_parameters();
    }
}

impl PluginStateImpl for ForgeClapMainThread<'_> {
    fn save(&mut self, output: &mut OutputStream) -> Result<(), PluginError> {
        output.write_all(STATE_MAGIC)?;
        for value in [
            self.shared.params.gain_db.load(),
            self.shared.params.ceiling_dbtp.load(),
            self.shared.params.attack_ms.load(),
            self.shared.params.release_ms.load(),
            self.shared.params.bypass.load(),
        ] {
            output.write_all(&value.to_le_bytes())?;
        }
        Ok(())
    }

    fn load(&mut self, input: &mut InputStream) -> Result<(), PluginError> {
        let mut magic = [0_u8; STATE_MAGIC.len()];
        input.read_exact(&mut magic)?;
        if &magic != STATE_MAGIC {
            return Err(PluginError::Message("Unsupported Forge Live state"));
        }
        let mut values = [0_f32; 5];
        for value in &mut values {
            let mut bytes = [0_u8; 4];
            input.read_exact(&mut bytes)?;
            *value = f32::from_le_bytes(bytes);
        }
        self.shared.params.set_values(ParamValues {
            gain_db: values[0],
            ceiling_dbtp: values[1],
            attack_ms: values[2],
            release_ms: values[3],
            bypass: values[4],
        });
        Ok(())
    }
}

struct AtomicF32(AtomicU32);

impl AtomicF32 {
    fn new(value: f32) -> Self {
        Self(AtomicU32::new(value.to_bits()))
    }

    fn store(&self, value: f32) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }

    fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

clack_plugin::clack_export_entry!(clack_plugin::entry::SinglePluginEntry<ForgeClapPlugin>);

#[cfg(test)]
mod tests {
    use super::*;
    use clack_host::events::event_types::ParamValueEvent;
    use clack_host::prelude::*;
    use clack_host::utils::Cookie;
    use clack_plugin::entry::SinglePluginEntry;

    #[test]
    fn plugin_parity_clap_host_output_matches_realtime_processor() {
        let info = HostInfo::new("forge-test", "", "", "").unwrap();
        let entry =
            PluginEntry::load_from_clack::<SinglePluginEntry<ForgeClapPlugin>>(c"").unwrap();
        let descriptor = entry
            .get_plugin_factory()
            .unwrap()
            .plugin_descriptor(0)
            .unwrap();
        assert_eq!(
            descriptor.id(),
            Some(c"io.github.penguin425.audio-normalizer.forge-live")
        );
        let mut plugin = PluginInstance::<TestHostHandlers>::new(
            |_| TestHostShared,
            |_| TestHostMainThread,
            &entry,
            descriptor.id().unwrap(),
            &info,
        )
        .unwrap();
        let processor = plugin
            .activate(
                |_, _| TestHostAudioProcessor,
                PluginAudioConfiguration {
                    sample_rate: 48_000.0,
                    min_frames_count: 512,
                    max_frames_count: 512,
                },
            )
            .unwrap();
        let mut input_events = EventBuffer::with_capacity(4);
        let mut output_events = EventBuffer::with_capacity(4);
        input_events.push(&ParamValueEvent::new(
            0,
            PARAM_GAIN,
            Pckn::match_all(),
            -6.0,
            Cookie::empty(),
        ));
        let left = (0..512)
            .map(|frame| {
                let carrier = (frame as f32 * 0.071).sin();
                let envelope = if (128..384).contains(&frame) {
                    1.35
                } else {
                    0.23
                };
                carrier * envelope
            })
            .collect::<Vec<_>>();
        let right = left
            .iter()
            .enumerate()
            .map(|(frame, sample)| {
                if frame.is_multiple_of(5) {
                    -*sample * 0.71
                } else {
                    *sample * 0.43
                }
            })
            .collect::<Vec<_>>();
        let mut expected = left
            .iter()
            .zip(&right)
            .flat_map(|(left, right)| [*left, *right])
            .collect::<Vec<_>>();
        let mut reference =
            RealtimeGainProcessor::new(48_000, 2, RealtimeGainConfig::default()).unwrap();
        reference.set_target_gain_db(-6.0).unwrap();
        reference.process_interleaved(&mut expected).unwrap();
        let mut inputs = [left, right];
        let mut outputs = [vec![0.0_f32; 512], vec![0.0_f32; 512]];
        let mut processor = processor.start_processing().unwrap();
        let mut input_ports = AudioPorts::with_capacity(2, 1);
        let mut output_ports = AudioPorts::with_capacity(2, 1);
        let input_buffers = input_ports.with_input_buffers([AudioPortBuffer {
            channels: AudioPortBufferType::f32_input_only(
                inputs.iter_mut().map(InputChannel::variable),
            ),
            latency: 0,
        }]);
        let mut output_buffers = output_ports.with_output_buffers([AudioPortBuffer {
            channels: AudioPortBufferType::f32_output_only(
                outputs.iter_mut().map(Vec::as_mut_slice),
            ),
            latency: 0,
        }]);
        processor
            .process(
                &input_buffers,
                &mut output_buffers,
                &input_events.as_input(),
                &mut output_events.as_output(),
                None,
                None,
            )
            .unwrap();
        let actual = outputs[0]
            .iter()
            .zip(&outputs[1])
            .flat_map(|(left, right)| [*left, *right])
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        plugin.deactivate(processor.stop_processing());
    }

    struct TestHostMainThread;
    struct TestHostShared;
    struct TestHostAudioProcessor;
    struct TestHostHandlers;

    impl SharedHandler<'_> for TestHostShared {
        fn request_restart(&self) {}
        fn request_process(&self) {}
        fn request_callback(&self) {}
    }
    impl AudioProcessorHandler<'_> for TestHostAudioProcessor {}
    impl MainThreadHandler<'_> for TestHostMainThread {}
    impl HostHandlers for TestHostHandlers {
        type Shared<'a> = TestHostShared;
        type MainThread<'a> = TestHostMainThread;
        type AudioProcessor<'a> = TestHostAudioProcessor;
    }
}
