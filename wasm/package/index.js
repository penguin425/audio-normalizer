import init, {
  analyze_interleaved_json,
  analyze_wav_json,
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

export function analyzeInterleaved(samples, sampleRate, channels) {
  return JSON.parse(analyze_interleaved_json(samples, sampleRate, channels));
}
