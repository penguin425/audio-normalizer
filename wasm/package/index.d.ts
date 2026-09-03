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
/** Accepts mono/stereo PCM. Multichannel PCM must use analyzeWav with a complete speaker mask. */
export function analyzeInterleaved(
  samples: Float32Array,
  sampleRate: number,
  channels: number,
): ForgeAnalysis;
