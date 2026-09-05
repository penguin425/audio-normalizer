import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import init, {
  analyzeInterleaved,
  analyzeInterleavedWithLayout,
  analyzeWav,
  analyzeWavWithLayout,
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
  maxImplicitLayoutChannels: 2,
  minSampleRate: 8000,
  maxSampleRate: 384000,
  requiresCompleteWaveLayout: true,
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

function floatWave(interleaved, rate, channels, channelMask) {
  const extensible = channelMask !== undefined;
  const formatSize = extensible ? 40 : 16;
  const dataHeaderOffset = 20 + formatSize;
  const dataOffset = dataHeaderOffset + 8;
  const wave = new Uint8Array(dataOffset + interleaved.byteLength);
  const view = new DataView(wave.buffer);

  for (const [offset, text] of [
    [0, "RIFF"],
    [8, "WAVE"],
    [12, "fmt "],
    [dataHeaderOffset, "data"],
  ]) {
    for (let index = 0; index < text.length; index += 1) {
      wave[offset + index] = text.charCodeAt(index);
    }
  }
  view.setUint32(4, wave.byteLength - 8, true);
  view.setUint32(16, formatSize, true);
  view.setUint16(20, extensible ? 0xfffe : 3, true);
  view.setUint16(22, channels, true);
  view.setUint32(24, rate, true);
  view.setUint32(28, rate * channels * 4, true);
  view.setUint16(32, channels * 4, true);
  view.setUint16(34, 32, true);
  if (extensible) {
    view.setUint16(36, 22, true);
    view.setUint16(38, 32, true);
    view.setUint32(40, channelMask, true);
    // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT.
    wave.set(
      [
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00,
        0xaa, 0x00, 0x38, 0x9b, 0x71,
      ],
      44,
    );
  }
  view.setUint32(dataHeaderOffset + 4, interleaved.byteLength, true);
  new Float32Array(wave.buffer, dataOffset).set(interleaved);
  return wave;
}

const wave = floatWave(samples, sampleRate, 1);
const waveResult = analyzeWav(wave);
assert.equal(waveResult.integratedLufs, result.integratedLufs);
assert.equal(waveResult.truePeakDbtp, result.truePeakDbtp);

assert.equal(
  analyzeWav(floatWave(new Float32Array(2), sampleRate, 2)).channels,
  2,
);
assert.equal(
  analyzeWav(floatWave(new Float32Array(6), sampleRate, 6, 0x003f))
    .channels,
  6,
);
const exactWave = analyzeWavWithLayout(
  floatWave(new Float32Array(6), sampleRate, 6, 0x003f),
);
assert.equal(exactWave.channelLayout.origin, "wave");
assert.equal(exactWave.channelLayout.wave_channel_mask, 0x003f);
for (const unknownLayoutWave of [
  floatWave(new Float32Array(3), sampleRate, 3),
  floatWave(new Float32Array(2), sampleRate, 2, 0),
  floatWave(new Float32Array(6), sampleRate, 6, 0x0003),
]) {
  assert.throws(
    () => analyzeWav(unknownLayoutWave),
    /WAVE channel layout is unknown.*complete speaker mask/,
  );
}
for (const nonFinite of [Number.NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY]) {
  assert.throws(
    () => analyzeWav(floatWave(Float32Array.of(0, nonFinite), sampleRate, 1)),
    /samples must contain only finite values/,
  );
}

assert.equal(
  analyzeInterleaved(new Float32Array(2), sampleRate, 2).channels,
  2,
);
assert.throws(
  () => analyzeInterleaved(new Float32Array(3), sampleRate, 3),
  /more than 2 channels requires an explicit layout.*analyzeWav/,
);

const threeChannelLayout = {
  version: 1,
  assignments: ["main", "main", "main"].map((role) => ({
    kind: "legacy-role",
    role,
  })),
  provenance: "known-speakers",
  origin: "explicit-override",
};
const exactInterleaved = analyzeInterleavedWithLayout(
  new Float32Array(3),
  sampleRate,
  3,
  threeChannelLayout,
);
assert.equal(exactInterleaved.channels, 3);
assert.equal(exactInterleaved.channelLayout.origin, "explicit-override");

assert.throws(
  () => analyzeInterleaved(new Float32Array(3), sampleRate, 2),
  /divisible by channels/,
);
assert.throws(
  () => analyzeInterleaved(Float32Array.of(Number.NaN), sampleRate, 1),
  /finite values/,
);
for (const supportedRate of [8000, 384000]) {
  assert.equal(
    analyzeInterleaved(Float32Array.of(0), supportedRate, 1).sampleRate,
    supportedRate,
  );
}
for (const unsupportedRate of [7999, 384001]) {
  assert.throws(
    () => analyzeInterleaved(Float32Array.of(0), unsupportedRate, 1),
    /sampleRate must be in 8000\.\.=384000/,
  );
}

console.log(
  JSON.stringify({
    version: version(),
    integratedLufs: result.integratedLufs,
    truePeakDbtp: result.truePeakDbtp,
  }),
);
