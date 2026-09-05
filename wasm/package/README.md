# Forge WebAssembly analysis

This dependency-free ES module runs Forge loudness analysis locally in a
browser. It performs no network or filesystem access and does not include
normalization or encoding.

```js
import init, { analyzeWav } from "./index.js";

await init();
const analysis = analyzeWav(new Uint8Array(await file.arrayBuffer()));
console.log(analysis.integratedLufs, analysis.truePeakDbtp);
```

`analyzeWavWithLayout(bytes, descriptor?)` returns the analysis plus its exact
`channelLayout`. `analyzeInterleavedWithLayout(samples, sampleRate, channels,
descriptor)` provides the same contract for decoded PCM. Caller-supplied
descriptors must use schema version 1, origin `explicit-override`, and identify
every physical speaker; object, Ambisonic, unknown, and incomplete layouts are
rejected before measurement.

`analyzeWav` accepts PCM or IEEE-float WAVE/RF64/BW64 data. Conventional mono
and stereo WAVE files have an implicit speaker layout; multichannel files must
use WAVE_FORMAT_EXTENSIBLE with a complete standard speaker mask.
`analyzeInterleaved` accepts mono or stereo decoded interleaved `Float32Array`
PCM, including audio decoded with the Web Audio API. Multichannel decoded PCM
uses `analyzeInterleavedWithLayout` with an exact override. `limits()` reports
both the 32-channel decoder ceiling and the two-channel implicit-layout
ceiling, along with the byte, sample, and sample-rate bounds enforced by these
entry points.

Loudness range is reported for short inputs, but `loudnessRangeStable` remains
false until the EBU Tech 3341 one-minute stability threshold is reached.
