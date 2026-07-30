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

`analyzeWav` accepts PCM or IEEE-float WAVE/RF64/BW64 data.
`analyzeInterleaved` accepts decoded interleaved `Float32Array` PCM, including
audio decoded with the Web Audio API. Both entry points enforce the resource
limits returned by `limits()`.

Loudness range is reported for short inputs, but `loudnessRangeStable` remains
false until the EBU Tech 3341 one-minute stability threshold is reached.
