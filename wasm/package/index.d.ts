export interface ForgeAnalysis {
  version: string;
  sampleRate: number;
  channels: number;
  frames: number;
  durationSeconds: number;
  integratedLufs: number | null;
  maxMomentaryLufs: number | null;
  maxShortTermLufs: number | null;
  loudnessRangeLu: number;
  loudnessRangeStable: boolean;
  rmsDbfs: number | null;
  samplePeakDbfs: number | null;
  truePeakDbtp: number | null;
  peakToLoudnessRatioLu: number | null;
}

export type ChannelRole =
  | "main"
  | "surround"
  | "dual-mono"
  | "lfe"
  | { positioned: { azimuth_degrees: number; elevation_degrees: number } };

export interface ChannelAssignment {
  kind:
    | "speaker"
    | "low-frequency-effects"
    | "legacy-role"
    | "unassigned"
    | "ambisonic"
    | "object";
  role: ChannelRole;
  cicp_position?: number;
  azimuth_degrees?: number;
  elevation_degrees?: number;
  component_index?: number;
}

export interface ChannelLayoutDescriptor {
  version: 1;
  assignments: ChannelAssignment[];
  provenance: "known-speakers" | "unknown" | "scene-based";
  origin:
    | "compatibility-default"
    | "wave"
    | "flac"
    | "iso-bmff"
    | "decoder"
    | "explicit-override"
    | "renderer";
  wave_channel_mask?: number;
  flac_channel_mask?: number;
  iso_bmff?: Record<string, unknown>;
  renderer?: Record<string, unknown>;
}

export interface ForgeAnalysisWithLayout extends ForgeAnalysis {
  channelLayout: ChannelLayoutDescriptor;
}

export interface ForgeWasmLimits {
  maxInputBytes: number;
  maxDecodedSamples: number;
  maxChannels: number;
  maxSampleRate: number;
}

export interface ForgeWasmLimitsV2 extends ForgeWasmLimits {
  maxImplicitLayoutChannels: number;
  minSampleRate: number;
  requiresCompleteWaveLayout: boolean;
}

export default function init(
  input?: {
    module_or_path?: RequestInfo | URL | Response | BufferSource | WebAssembly.Module;
  },
): Promise<unknown>;
export function version(): string;
export function limits(): ForgeWasmLimitsV2;
export function analyzeWav(bytes: Uint8Array): ForgeAnalysis;
export function analyzeWavWithLayout(
  bytes: Uint8Array,
  channelLayout?: ChannelLayoutDescriptor | string,
): ForgeAnalysisWithLayout;
/** Accepts mono/stereo PCM. Multichannel PCM must use analyzeWav with a complete speaker mask. */
export function analyzeInterleaved(
  samples: Float32Array,
  sampleRate: number,
  channels: number,
): ForgeAnalysis;
export function analyzeInterleavedWithLayout(
  samples: Float32Array,
  sampleRate: number,
  channels: number,
  channelLayout?: ChannelLayoutDescriptor | string,
): ForgeAnalysisWithLayout;
