// Forge CUDA true-peak chunk kernel.
//
// The caller pads every channel with the preceding 15 samples. One CUDA
// thread reconstructs every oversampled phase for one input frame, then an
// unsigned atomic maximum retains the non-negative f32 peak bits per channel.
// Coefficients are supplied by Rust from the same normalized phase table used
// by the CPU meter, and explicit f64 fma() preserves the CPU AVX2/FMA order.

extern "C" __global__ void forge_true_peak_chunk(
    const float *samples,
    const double *coefficients,
    unsigned long long stride,
    unsigned int frames,
    unsigned int channels,
    unsigned int factor,
    unsigned int *channel_peaks) {
    const unsigned int frame = blockIdx.x * blockDim.x + threadIdx.x;
    const unsigned int channel = blockIdx.y;
    if (frame >= frames || channel >= channels) {
        return;
    }

    const unsigned long long current =
        (unsigned long long)channel * stride + 15ull + frame;
    unsigned int peak_bits = __float_as_uint(samples[current]) & 0x7fffffffu;
    float peak = __uint_as_float(peak_bits);
    if (isnan(peak)) {
        peak = 0.0f;
    }

    double phase0 = 0.0;
    double phase1 = 0.0;
    double phase2 = 0.0;
    double phase3 = 0.0;
    #pragma unroll
    for (unsigned int tap = 0; tap < 16; ++tap) {
        const double sample = (double)samples[current - tap];
        const unsigned int coefficient = tap * 4;
        phase0 = fma(coefficients[coefficient], sample, phase0);
        phase1 = fma(coefficients[coefficient + 1], sample, phase1);
        if (factor == 4) {
            phase2 = fma(coefficients[coefficient + 2], sample, phase2);
            phase3 = fma(coefficients[coefficient + 3], sample, phase3);
        }
    }

    peak = fmaxf(peak, (float)fabs(phase0));
    peak = fmaxf(peak, (float)fabs(phase1));
    if (factor == 4) {
        peak = fmaxf(peak, (float)fabs(phase2));
        peak = fmaxf(peak, (float)fabs(phase3));
    }
    atomicMax(channel_peaks + channel, __float_as_uint(peak));
}
