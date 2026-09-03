# Forge Python bindings

`forge-normalizer` is the dependency-free Python interface to the versioned
Forge C ABI. Official wheels bundle the matching native analysis library for
Linux x86-64, macOS ARM64, macOS x86-64, or Windows x86-64.

```python
from forge_normalizer import analyze_file

analysis = analyze_file(
    "programme.wav",
    max_decoded_samples=48_000 * 2 * 60 * 60,
)
print(analysis.integrated_lufs, analysis.true_peak_dbtp)
```

The decoded-sample limit is mandatory and counts frames multiplied by
channels. File analysis rejects ambiguous or scene-based multichannel layouts
because C ABI v1 has no speaker-layout override. See
[`PYTHON-API.md`](https://github.com/penguin425/audio-normalizer/blob/main/PYTHON-API.md)
for installation, library selection, exceptions, compatibility, and the full
result schema.
