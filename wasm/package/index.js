import init, {
  analyze_interleaved_json,
  analyze_interleaved_with_layout_json,
  analyze_wav_json,
  analyze_wav_with_layout_json,
  limits_json,
  version,
} from "./forge_normalizer_wasm.js";

export default init;
export { init, version };

export function limits() {
  return JSON.parse(limits_json());
}

export function analyzeWav(bytes) {
  return JSON.parse(analyze_wav_json(bytes));
}

function layoutJson(layout) {
  if (layout == null) return undefined;
  return typeof layout === "string" ? layout : JSON.stringify(layout);
}

export function analyzeWavWithLayout(bytes, channelLayout = undefined) {
  return JSON.parse(
    analyze_wav_with_layout_json(bytes, layoutJson(channelLayout)),
  );
}

export function analyzeInterleaved(samples, sampleRate, channels) {
  return JSON.parse(analyze_interleaved_json(samples, sampleRate, channels));
}

export function analyzeInterleavedWithLayout(
  samples,
  sampleRate,
  channels,
  channelLayout = undefined,
) {
  return JSON.parse(
    analyze_interleaved_with_layout_json(
      samples,
      sampleRate,
      channels,
      layoutJson(channelLayout),
    ),
  );
}
