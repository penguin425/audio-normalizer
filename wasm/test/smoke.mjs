import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import init, {
  analyzeInterleaved,
  analyzeWav,
  limits,
  version,
} from "../package/index.js";

const moduleBytes = await readFile(
  new URL("../package/forge_normalizer_wasm_bg.wasm", import.meta.url),
);
await init({ module_or_path: moduleBytes });

assert.match(version(), /^\d+\.\d+\.\d+$/);
assert.deepEqual(limits(), {
  maxInputBytes: 134217728,
  maxDecodedSamples: 24000000,
  maxChannels: 32,
  maxSampleRate: 768000,
});

const sampleRate = 48000;
const samples = Float32Array.from(
  { length: sampleRate },
  (_, index) => 0.1 * Math.sin((2 * Math.PI * 1000 * index) / sampleRate),
);
const result = analyzeInterleaved(samples, sampleRate, 1);
assert.equal(result.sampleRate, sampleRate);
assert.equal(result.channels, 1);
assert.equal(result.frames, sampleRate);
assert.ok(Number.isFinite(result.integratedLufs));
assert.ok(Number.isFinite(result.truePeakDbtp));

const wave = new Uint8Array(44 + samples.byteLength);
const view = new DataView(wave.buffer);
for (const [offset, text] of [
  [0, "RIFF"],
  [8, "WAVE"],
  [12, "fmt "],
  [36, "data"],
]) {
  for (let index = 0; index < text.length; index += 1) {
    wave[offset + index] = text.charCodeAt(index);
  }
}
view.setUint32(4, wave.byteLength - 8, true);
view.setUint32(16, 16, true);
view.setUint16(20, 3, true);
view.setUint16(22, 1, true);
view.setUint32(24, sampleRate, true);
view.setUint32(28, sampleRate * 4, true);
view.setUint16(32, 4, true);
view.setUint16(34, 32, true);
view.setUint32(40, samples.byteLength, true);
new Float32Array(wave.buffer, 44).set(samples);
const waveResult = analyzeWav(wave);
assert.equal(waveResult.integratedLufs, result.integratedLufs);
assert.equal(waveResult.truePeakDbtp, result.truePeakDbtp);

assert.throws(
  () => analyzeInterleaved(new Float32Array(3), sampleRate, 2),
  /divisible by channels/,
);
assert.throws(
  () => analyzeInterleaved(Float32Array.of(Number.NaN), sampleRate, 1),
  /finite values/,
);

console.log(
  JSON.stringify({
    version: version(),
    integratedLufs: result.integratedLufs,
    truePeakDbtp: result.truePeakDbtp,
  }),
);
