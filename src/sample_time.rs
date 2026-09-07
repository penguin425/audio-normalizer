//! Exact sample-coordinate transformations between sample rates.
//!
//! Sample-rate conversion changes the integer coordinate space used by a
//! number of container metadata fields.  This module keeps those operations
//! in integer arithmetic so that metadata does not acquire a floating-point
//! rounding error while an audio stream is being resampled.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// Rounding policy for a point coordinate or a frame count.
///
/// [`HalfUp`](Self::HalfUp) is deliberately the default.  It is the policy
/// used by Forge's existing output-length calculation (`n * out / input`,
/// rounded to the nearest integer with exact half values rounded upward), so
/// centralising the calculation does not change existing output lengths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum RoundingMode {
    /// Round to the nearest integer; an exact half is rounded away from zero.
    ///
    /// For signed values this is implemented symmetrically by rounding the
    /// magnitude and restoring the sign.
    #[default]
    HalfUp,
    /// Round to the nearest integer; an exact half is rounded to the even
    /// integer.  This is available for metadata callers that need an unbiased
    /// tie rule, while the resampler continues to use [`RoundingMode::HalfUp`].
    TiesToEven,
}

/// Errors returned by exact sample-coordinate operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SampleTimeError {
    /// A source or destination sample rate was zero.
    ZeroRate { source_rate: u32, target_rate: u32 },
    /// An intermediate product or final integer representation overflowed.
    Overflow { operation: &'static str },
    /// A half-open span has its end before its start.
    InvalidSpan { start: u64, end: u64 },
    /// A coordinate cannot be made relative to a crop origin because it is
    /// before that origin.
    CropOriginAfterCoordinate { coordinate: u64, origin: u64 },
}

impl fmt::Display for SampleTimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroRate {
                source_rate,
                target_rate,
            } => write!(
                formatter,
                "sample rates must be positive (source={source_rate}, target={target_rate})"
            ),
            Self::Overflow { operation } => {
                write!(formatter, "sample-time {operation} overflow")
            }
            Self::InvalidSpan { start, end } => {
                write!(formatter, "sample span end {end} precedes start {start}")
            }
            Self::CropOriginAfterCoordinate { coordinate, origin } => write!(
                formatter,
                "sample coordinate {coordinate} precedes crop origin {origin}"
            ),
        }
    }
}

impl std::error::Error for SampleTimeError {}

/// A half-open sample span `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[non_exhaustive]
pub struct SampleSpan {
    start: u64,
    end: u64,
}

impl SampleSpan {
    /// Construct a half-open span after validating its endpoint order.
    pub const fn new(start: u64, end: u64) -> Result<Self, SampleTimeError> {
        if end < start {
            Err(SampleTimeError::InvalidSpan { start, end })
        } else {
            Ok(Self { start, end })
        }
    }

    /// Number of source samples in this span.
    pub const fn len(self) -> u64 {
        self.end - self.start
    }

    /// Whether this span contains no samples.
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// Start coordinate of this span.
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Exclusive end coordinate of this span.
    pub const fn end(self) -> u64 {
        self.end
    }
}

impl<'de> Deserialize<'de> for SampleSpan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        #[serde(rename_all = "kebab-case")]
        struct Fields {
            start: u64,
            end: u64,
        }
        let fields = Fields::deserialize(deserializer)?;
        Self::new(fields.start, fields.end).map_err(serde::de::Error::custom)
    }
}

/// An exact rational mapping from a source sample-rate coordinate space into
/// a target sample-rate coordinate space.
///
/// Rates remain in their original (unreduced) `u32` representation.  All
/// products are checked in `u128`, and public methods check conversion back to
/// their requested integer width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub struct SampleTimeTransform {
    source_rate: u32,
    target_rate: u32,
    /// Numerator/denominator after GCD reduction. These remain private so the
    /// serialized/public representation continues to expose the caller's
    /// original rates.
    #[serde(skip)]
    reduced_source_rate: u32,
    #[serde(skip)]
    reduced_target_rate: u32,
}

impl<'de> Deserialize<'de> for SampleTimeTransform {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        #[serde(rename_all = "kebab-case")]
        struct Fields {
            source_rate: u32,
            target_rate: u32,
        }
        let fields = Fields::deserialize(deserializer)?;
        Self::new(fields.source_rate, fields.target_rate).map_err(serde::de::Error::custom)
    }
}

/// Alias emphasizing that this value represents a rate ratio.
pub type SampleRateRatio = SampleTimeTransform;

/// Alias for callers that prefer mapper terminology.
pub type SampleTimeMapper = SampleTimeTransform;

const fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

impl SampleTimeTransform {
    /// Create a checked source-to-target sample-rate transform.
    pub const fn new(source_rate: u32, target_rate: u32) -> Result<Self, SampleTimeError> {
        if source_rate == 0 || target_rate == 0 {
            Err(SampleTimeError::ZeroRate {
                source_rate,
                target_rate,
            })
        } else {
            let divisor = gcd(source_rate, target_rate);
            Ok(Self {
                source_rate,
                target_rate,
                reduced_source_rate: source_rate / divisor,
                reduced_target_rate: target_rate / divisor,
            })
        }
    }

    pub const fn source_rate(self) -> u32 {
        self.source_rate
    }

    pub const fn target_rate(self) -> u32 {
        self.target_rate
    }

    /// Map a non-negative sample coordinate with the default [`RoundingMode::HalfUp`]
    /// policy.
    pub fn map_sample(self, coordinate: u64) -> Result<u64, SampleTimeError> {
        self.map_sample_with_rounding(coordinate, RoundingMode::HalfUp)
    }

    /// Map a non-negative sample coordinate with an explicit tie policy.
    pub fn map_sample_with_rounding(
        self,
        coordinate: u64,
        rounding: RoundingMode,
    ) -> Result<u64, SampleTimeError> {
        let mapped = self.map_u128_with_rounding(u128::from(coordinate), rounding)?;
        u64::try_from(mapped).map_err(|_| SampleTimeError::Overflow {
            operation: "coordinate conversion",
        })
    }

    /// Map a signed sample coordinate symmetrically around zero.
    pub fn map_signed_sample(self, coordinate: i64) -> Result<i64, SampleTimeError> {
        self.map_signed_sample_with_rounding(coordinate, RoundingMode::HalfUp)
    }

    /// Map a signed sample coordinate with an explicit tie policy.
    pub fn map_signed_sample_with_rounding(
        self,
        coordinate: i64,
        rounding: RoundingMode,
    ) -> Result<i64, SampleTimeError> {
        let mapped = self.map_signed_i128_with_rounding(i128::from(coordinate), rounding)?;
        i64::try_from(mapped).map_err(|_| SampleTimeError::Overflow {
            operation: "signed coordinate conversion",
        })
    }

    /// Map an `i128` signed coordinate without overflowing on `i128::MIN`.
    pub fn map_signed_i128(self, coordinate: i128) -> Result<i128, SampleTimeError> {
        self.map_signed_i128_with_rounding(coordinate, RoundingMode::HalfUp)
    }

    /// Map an `i128` signed coordinate with an explicit tie policy.
    pub fn map_signed_i128_with_rounding(
        self,
        coordinate: i128,
        rounding: RoundingMode,
    ) -> Result<i128, SampleTimeError> {
        let negative = coordinate < 0;
        let magnitude = coordinate.unsigned_abs();
        let mapped = self.map_u128_with_rounding(magnitude, rounding)?;
        if negative {
            if mapped > (1_u128 << 127) {
                return Err(SampleTimeError::Overflow {
                    operation: "signed coordinate conversion",
                });
            }
            if mapped == (1_u128 << 127) {
                Ok(i128::MIN)
            } else {
                // The range check above makes this cast lossless.
                Ok(-(mapped as i128))
            }
        } else {
            i128::try_from(mapped).map_err(|_| SampleTimeError::Overflow {
                operation: "signed coordinate conversion",
            })
        }
    }

    /// Map a source frame count using the same positive half-up rule as the
    /// historical resampler output-length calculation.
    pub fn map_frame_count(self, frames: usize) -> Result<usize, SampleTimeError> {
        self.map_frame_count_with_rounding(frames, RoundingMode::HalfUp)
    }

    /// Map a source frame count with an explicit tie policy.
    pub fn map_frame_count_with_rounding(
        self,
        frames: usize,
        rounding: RoundingMode,
    ) -> Result<usize, SampleTimeError> {
        let mapped = self.map_u128_with_rounding(frames as u128, rounding)?;
        usize::try_from(mapped).map_err(|_| SampleTimeError::Overflow {
            operation: "frame-count conversion",
        })
    }

    /// Map a source coordinate by flooring its rational value.
    pub fn map_floor(self, coordinate: u64) -> Result<u64, SampleTimeError> {
        let mapped = self.map_floor_u128(u128::from(coordinate))?;
        u64::try_from(mapped).map_err(|_| SampleTimeError::Overflow {
            operation: "floor coordinate conversion",
        })
    }

    /// Map a source coordinate by ceiling its rational value.
    pub fn map_ceil(self, coordinate: u64) -> Result<u64, SampleTimeError> {
        let mapped = self.map_ceil_u128(u128::from(coordinate))?;
        u64::try_from(mapped).map_err(|_| SampleTimeError::Overflow {
            operation: "ceiling coordinate conversion",
        })
    }

    /// `map_floor`, retaining a `usize` representation for bounded DSP
    /// capacities.
    pub fn map_floor_usize(self, coordinate: usize) -> Result<usize, SampleTimeError> {
        let mapped = self.map_floor_u128(coordinate as u128)?;
        usize::try_from(mapped).map_err(|_| SampleTimeError::Overflow {
            operation: "floor coordinate conversion",
        })
    }

    /// `map_ceil`, retaining a `usize` representation for bounded DSP
    /// capacities.
    pub fn map_ceil_usize(self, coordinate: usize) -> Result<usize, SampleTimeError> {
        let mapped = self.map_ceil_u128(coordinate as u128)?;
        usize::try_from(mapped).map_err(|_| SampleTimeError::Overflow {
            operation: "ceiling coordinate conversion",
        })
    }

    /// Map a half-open span.  The start is floored and the end is ceiled so a
    /// resampled interval never silently loses source coverage.
    pub fn map_span(self, span: SampleSpan) -> Result<SampleSpan, SampleTimeError> {
        if span.end < span.start {
            return Err(SampleTimeError::InvalidSpan {
                start: span.start,
                end: span.end,
            });
        }
        // A zero-length interval has no covered samples.  Applying the
        // coverage-preserving floor/ceil rules independently to the same
        // endpoint would manufacture a non-empty target interval whenever
        // the endpoint is not exactly representable at the destination
        // rate.  Keep the interval empty while retaining the mapped left
        // boundary as its coordinate.
        if span.is_empty() {
            let mapped = self.map_floor(span.start)?;
            return SampleSpan::new(mapped, mapped);
        }
        let start = self.map_floor(span.start)?;
        let end = self.map_ceil(span.end)?;
        SampleSpan::new(start, end)
    }

    /// Map both endpoints with the point-coordinate rounding policy.  This is
    /// useful when the caller's schema defines rounded endpoint coordinates
    /// rather than an interval that must preserve coverage.
    pub fn map_span_with_rounding(
        self,
        span: SampleSpan,
        rounding: RoundingMode,
    ) -> Result<SampleSpan, SampleTimeError> {
        if span.end < span.start {
            return Err(SampleTimeError::InvalidSpan {
                start: span.start,
                end: span.end,
            });
        }
        let start = self.map_sample_with_rounding(span.start, rounding)?;
        let end = self.map_sample_with_rounding(span.end, rounding)?;
        SampleSpan::new(start, end)
    }

    /// Map a source duration/count with the default half-up policy.
    pub fn map_duration(self, duration: u64) -> Result<u64, SampleTimeError> {
        self.map_sample(duration)
    }

    /// Map a source crop origin into the target coordinate space.
    pub fn map_crop_origin(self, source_origin: u64) -> Result<u64, SampleTimeError> {
        self.map_sample(source_origin)
    }

    /// Map a signed crop origin, including origins before source sample zero.
    pub fn map_signed_crop_origin(self, source_origin: i128) -> Result<i128, SampleTimeError> {
        self.map_signed_i128(source_origin)
    }

    /// Convert a source coordinate into a target coordinate relative to a
    /// crop beginning at `source_origin`.
    pub fn map_relative_coordinate(
        self,
        source_coordinate: u64,
        source_origin: u64,
    ) -> Result<u64, SampleTimeError> {
        let relative = source_coordinate.checked_sub(source_origin).ok_or(
            SampleTimeError::CropOriginAfterCoordinate {
                coordinate: source_coordinate,
                origin: source_origin,
            },
        )?;
        self.map_sample(relative)
    }

    /// Convert a signed source coordinate into a target coordinate relative to
    /// a signed crop origin.  The subtraction is checked independently from
    /// the rate mapping so a malformed crop cannot wrap around the clock.
    pub fn map_signed_relative_coordinate(
        self,
        source_coordinate: i128,
        source_origin: i128,
    ) -> Result<i128, SampleTimeError> {
        let relative =
            source_coordinate
                .checked_sub(source_origin)
                .ok_or(SampleTimeError::Overflow {
                    operation: "crop subtraction",
                })?;
        self.map_signed_i128(relative)
    }

    /// Map a span after moving its source coordinate origin to a crop start.
    pub fn map_crop_span(
        self,
        span: SampleSpan,
        source_origin: u64,
    ) -> Result<SampleSpan, SampleTimeError> {
        if span.end < span.start {
            return Err(SampleTimeError::InvalidSpan {
                start: span.start,
                end: span.end,
            });
        }
        let start = span.start.checked_sub(source_origin).ok_or(
            SampleTimeError::CropOriginAfterCoordinate {
                coordinate: span.start,
                origin: source_origin,
            },
        )?;
        let end = span.end.checked_sub(source_origin).ok_or(
            SampleTimeError::CropOriginAfterCoordinate {
                coordinate: span.end,
                origin: source_origin,
            },
        )?;
        self.map_span(SampleSpan::new(start, end)?)
    }

    fn map_u128_with_rounding(
        self,
        coordinate: u128,
        rounding: RoundingMode,
    ) -> Result<u128, SampleTimeError> {
        let (quotient, remainder, denominator) = self.scaled_quotient_remainder(coordinate)?;
        let increment = match rounding {
            RoundingMode::HalfUp => remainder >= denominator - remainder,
            RoundingMode::TiesToEven => {
                remainder > denominator - remainder
                    || (remainder == denominator - remainder && quotient % 2 == 1)
            }
        };
        if increment {
            quotient.checked_add(1).ok_or(SampleTimeError::Overflow {
                operation: "coordinate rounding",
            })
        } else {
            Ok(quotient)
        }
    }

    fn map_floor_u128(self, coordinate: u128) -> Result<u128, SampleTimeError> {
        self.scaled_quotient_remainder(coordinate)
            .map(|(quotient, _, _)| quotient)
    }

    fn map_ceil_u128(self, coordinate: u128) -> Result<u128, SampleTimeError> {
        let (quotient, remainder, _) = self.scaled_quotient_remainder(coordinate)?;
        if remainder == 0 {
            Ok(quotient)
        } else {
            quotient.checked_add(1).ok_or(SampleTimeError::Overflow {
                operation: "coordinate ceiling",
            })
        }
    }

    /// Return floor(coordinate * target / source), its rational remainder,
    /// and the reduced denominator without constructing the potentially
    /// overflowing full product. Splitting `coordinate` by the denominator
    /// first makes every intermediate representable exactly when the final
    /// floor value itself is representable.
    fn scaled_quotient_remainder(
        self,
        coordinate: u128,
    ) -> Result<(u128, u128, u128), SampleTimeError> {
        let numerator = u128::from(self.reduced_target_rate);
        let denominator = u128::from(self.reduced_source_rate);
        let whole = coordinate / denominator;
        let coordinate_remainder = coordinate % denominator;
        let whole_scaled = whole
            .checked_mul(numerator)
            .ok_or(SampleTimeError::Overflow {
                operation: "coordinate multiplication",
            })?;
        // Both factors are below 2^32 after rate reduction, so this product
        // is far below the u128 ceiling.
        let fractional_numerator = coordinate_remainder * numerator;
        let fractional_quotient = fractional_numerator / denominator;
        let quotient =
            whole_scaled
                .checked_add(fractional_quotient)
                .ok_or(SampleTimeError::Overflow {
                    operation: "coordinate multiplication",
                })?;
        Ok((quotient, fractional_numerator % denominator, denominator))
    }
}

/// Calculate the target frame count using the historical positive half-up
/// rule, with checked rates and checked conversion to `usize`.
pub fn output_frame_count(
    input_frames: usize,
    input_rate: u32,
    output_rate: u32,
) -> Result<usize, SampleTimeError> {
    output_frame_count_with_rounding(input_frames, input_rate, output_rate, RoundingMode::HalfUp)
}

/// Calculate the target frame count with an explicit rounding policy.
pub fn output_frame_count_with_rounding(
    input_frames: usize,
    input_rate: u32,
    output_rate: u32,
    rounding: RoundingMode,
) -> Result<usize, SampleTimeError> {
    SampleTimeTransform::new(input_rate, output_rate)?
        .map_frame_count_with_rounding(input_frames, rounding)
}

/// Descriptive alias for [`output_frame_count`].
pub fn resampled_frame_count(
    input_frames: usize,
    input_rate: u32,
    output_rate: u32,
) -> Result<usize, SampleTimeError> {
    output_frame_count(input_frames, input_rate, output_rate)
}

/// Map one non-negative sample index with the default half-up rule.
pub fn map_sample_index(
    source_index: u64,
    source_rate: u32,
    target_rate: u32,
) -> Result<u64, SampleTimeError> {
    SampleTimeTransform::new(source_rate, target_rate)?.map_sample(source_index)
}

/// Map one signed sample coordinate with the default symmetric half-up rule.
pub fn map_signed_sample_index(
    source_index: i64,
    source_rate: u32,
    target_rate: u32,
) -> Result<i64, SampleTimeError> {
    SampleTimeTransform::new(source_rate, target_rate)?.map_signed_sample(source_index)
}

/// Map an arbitrary signed sample coordinate without narrowing it to `i64`.
pub fn map_signed_sample_coordinate(
    source_coordinate: i128,
    source_rate: u32,
    target_rate: u32,
) -> Result<i128, SampleTimeError> {
    SampleTimeTransform::new(source_rate, target_rate)?.map_signed_i128(source_coordinate)
}

/// Map a half-open source span, flooring the start and ceiling the end.
pub fn map_sample_span(
    start: u64,
    end: u64,
    source_rate: u32,
    target_rate: u32,
) -> Result<SampleSpan, SampleTimeError> {
    SampleTimeTransform::new(source_rate, target_rate)?.map_span(SampleSpan::new(start, end)?)
}

/// Map a source crop origin with the default half-up rule.
pub fn map_crop_origin(
    source_origin: u64,
    source_rate: u32,
    target_rate: u32,
) -> Result<u64, SampleTimeError> {
    SampleTimeTransform::new(source_rate, target_rate)?.map_crop_origin(source_origin)
}

/// Map an arbitrary signed crop origin.
pub fn map_signed_crop_origin(
    source_origin: i128,
    source_rate: u32,
    target_rate: u32,
) -> Result<i128, SampleTimeError> {
    SampleTimeTransform::new(source_rate, target_rate)?.map_signed_crop_origin(source_origin)
}

/// Map a source coordinate relative to an unsigned crop origin.
pub fn map_relative_sample_coordinate(
    source_coordinate: u64,
    source_origin: u64,
    source_rate: u32,
    target_rate: u32,
) -> Result<u64, SampleTimeError> {
    SampleTimeTransform::new(source_rate, target_rate)?
        .map_relative_coordinate(source_coordinate, source_origin)
}

/// Map a source coordinate relative to a signed crop origin.
pub fn map_relative_signed_sample_coordinate(
    source_coordinate: i128,
    source_origin: i128,
    source_rate: u32,
    target_rate: u32,
) -> Result<i128, SampleTimeError> {
    SampleTimeTransform::new(source_rate, target_rate)?
        .map_signed_relative_coordinate(source_coordinate, source_origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_length_preserves_legacy_half_up_ties() {
        // 1 * 3 / 2 = 1.5; the old `(n * out + input / 2) / input`
        // expression returned two.
        assert_eq!(output_frame_count(1, 2, 3), Ok(2));
        assert_eq!(output_frame_count(3, 2, 3), Ok(5));
        assert_eq!(output_frame_count(0, 48_000, 44_100), Ok(0));
    }

    #[test]
    fn ties_to_even_is_available_without_changing_the_default() {
        let transform = SampleTimeTransform::new(2, 3).unwrap();
        assert_eq!(
            transform.map_sample_with_rounding(1, RoundingMode::HalfUp),
            Ok(2)
        );
        assert_eq!(
            transform.map_sample_with_rounding(1, RoundingMode::TiesToEven),
            Ok(2)
        );
        assert_eq!(
            SampleTimeTransform::new(2, 5)
                .unwrap()
                .map_sample_with_rounding(1, RoundingMode::TiesToEven),
            Ok(2)
        );
    }

    #[test]
    fn signed_minimum_coordinate_is_checked_and_symmetric() {
        let transform = SampleTimeTransform::new(2, 3).unwrap();
        assert_eq!(transform.map_signed_sample(-1), Ok(-2));
        assert!(matches!(
            transform.map_signed_i128(i128::MIN),
            Err(SampleTimeError::Overflow { .. })
        ));
        assert_eq!(
            SampleTimeTransform::new(2, 1)
                .unwrap()
                .map_signed_i128(i128::MIN),
            Ok(i128::MIN / 2)
        );
        assert_eq!(
            SampleTimeTransform::new(u32::MAX, u32::MAX)
                .unwrap()
                .map_signed_i128(i128::MIN),
            Ok(i128::MIN)
        );
    }

    #[test]
    fn gcd_reduction_prevents_unnecessary_large_coordinate_overflow() {
        // The unreduced product is larger than u128 for i128::MAX, while the
        // mathematically equivalent 1/2 ratio is representable.
        let transform = SampleTimeTransform::new(4_000_000_000, 2_000_000_000).unwrap();
        assert_eq!(
            transform.map_signed_i128(i128::MAX),
            Ok((i128::MAX / 2) + 1)
        );
        assert_eq!(transform.map_sample(u64::MAX), Ok((u64::MAX / 2) + 1));

        // Reducing the rates alone is insufficient for 3/4: the direct
        // magnitude*3 product overflows u128 even though the mapped signed
        // coordinate is well inside i128.
        let three_quarters = SampleTimeTransform::new(4, 3).unwrap();
        assert_eq!(
            three_quarters.map_signed_i128(i128::MIN),
            Ok((i128::MIN / 4) * 3)
        );
        assert_eq!(
            three_quarters.map_signed_i128(i128::MAX),
            Ok((i128::MAX / 4) * 3 + 2)
        );
    }

    #[test]
    fn gcd_reduction_keeps_rounding_floor_and_ceil_semantics() {
        let transform = SampleTimeTransform::new(6, 4).unwrap();
        assert_eq!(
            transform.map_sample_with_rounding(1, RoundingMode::HalfUp),
            Ok(1)
        );
        assert_eq!(
            transform.map_sample_with_rounding(1, RoundingMode::TiesToEven),
            Ok(1)
        );
        assert_eq!(transform.map_floor(1), Ok(0));
        assert_eq!(transform.map_ceil(1), Ok(1));
        assert_eq!(
            transform.map_span(SampleSpan::new(1, 2).unwrap()),
            Ok(SampleSpan::new(0, 2).unwrap())
        );
    }

    #[test]
    fn endpoint_spans_preserve_coverage() {
        let transform = SampleTimeTransform::new(3, 2).unwrap();
        assert_eq!(
            transform.map_span(SampleSpan { start: 1, end: 2 }),
            Ok(SampleSpan { start: 0, end: 2 })
        );
        assert_eq!(
            SampleSpan::new(2, 1),
            Err(SampleTimeError::InvalidSpan { start: 2, end: 1 })
        );
        assert_eq!(
            transform.map_span(SampleSpan::new(1, 1).unwrap()),
            Ok(SampleSpan::new(0, 0).unwrap())
        );
    }

    #[test]
    fn crop_origin_is_applied_before_rate_mapping() {
        let transform = SampleTimeTransform::new(48_000, 44_100).unwrap();
        assert_eq!(transform.map_crop_origin(48_000), Ok(44_100));
        assert_eq!(transform.map_relative_coordinate(48_001, 48_000), Ok(1));
        assert_eq!(
            transform.map_relative_coordinate(47_999, 48_000),
            Err(SampleTimeError::CropOriginAfterCoordinate {
                coordinate: 47_999,
                origin: 48_000,
            })
        );
        assert_eq!(
            transform.map_signed_relative_coordinate(-47_999, -48_000),
            Ok(1)
        );
        assert_eq!(
            transform.map_signed_relative_coordinate(i128::MAX, i128::MIN),
            Err(SampleTimeError::Overflow {
                operation: "crop subtraction",
            })
        );
    }

    #[test]
    fn zero_rates_and_integer_overflow_are_errors() {
        assert_eq!(
            SampleTimeTransform::new(0, 48_000),
            Err(SampleTimeError::ZeroRate {
                source_rate: 0,
                target_rate: 48_000,
            })
        );
        assert!(matches!(
            SampleTimeTransform::new(1, u32::MAX)
                .unwrap()
                .map_sample(u64::MAX),
            Err(SampleTimeError::Overflow {
                operation: "coordinate conversion"
            })
        ));
    }

    #[test]
    fn serde_round_trip_keeps_checked_invariants() {
        let transform = SampleTimeTransform::new(48_000, 44_100).unwrap();
        let encoded = serde_json::to_string(&transform).unwrap();
        assert_eq!(encoded, r#"{"source-rate":48000,"target-rate":44100}"#);
        assert_eq!(
            serde_json::from_str::<SampleTimeTransform>(&encoded).unwrap(),
            transform
        );
        assert!(serde_json::from_str::<SampleTimeTransform>(
            r#"{"source-rate":0,"target-rate":48000}"#
        )
        .is_err());

        let span = SampleSpan::new(1, 2).unwrap();
        let encoded = serde_json::to_string(&span).unwrap();
        assert_eq!(serde_json::from_str::<SampleSpan>(&encoded).unwrap(), span);
        assert!(serde_json::from_str::<SampleSpan>(r#"{"start":2,"end":1}"#).is_err());
    }
}
