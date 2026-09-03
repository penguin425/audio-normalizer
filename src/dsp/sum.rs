//! Deterministic compensated and block-local floating-point accumulation.

/// Mergeable Neumaier compensated sum.
///
/// The correction retains low-order terms that ordinary left-to-right
/// addition loses when programme energy spans a large dynamic range. Callers
/// still choose an explicit merge order; this type never performs an
/// unordered parallel reduction.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct CompensatedSum {
    sum: f64,
    correction: f64,
}

impl CompensatedSum {
    pub(crate) const fn new() -> Self {
        Self {
            sum: 0.0,
            correction: 0.0,
        }
    }

    #[inline]
    pub(crate) fn add(&mut self, value: f64) {
        let next = self.sum + value;
        if self.sum.abs() >= value.abs() {
            self.correction += (self.sum - next) + value;
        } else {
            self.correction += (value - next) + self.sum;
        }
        self.sum = next;
    }

    #[inline]
    pub(crate) fn subtract(&mut self, value: f64) {
        self.add(-value);
    }

    /// Update a bounded rolling sum in its declared left-to-right order.
    ///
    /// Callers periodically replace the value with a compensated rebase, so
    /// this avoids paying the Neumaier dependency chain for every audio frame
    /// without allowing lifetime rounding drift to grow without bound.
    #[inline(always)]
    pub(crate) fn add_ordered(&mut self, value: f64) {
        debug_assert_eq!(self.correction, 0.0);
        self.sum += value;
    }

    #[inline(always)]
    pub(crate) fn subtract_ordered(&mut self, value: f64) {
        debug_assert_eq!(self.correction, 0.0);
        self.sum -= value;
    }

    #[inline(always)]
    pub(crate) fn ordered_total(self) -> f64 {
        debug_assert_eq!(self.correction, 0.0);
        self.sum
    }

    pub(crate) fn reset_ordered(&mut self, value: f64) {
        self.sum = value;
        self.correction = 0.0;
    }

    /// Merge another partial in a caller-defined order.
    #[inline]
    pub(crate) fn merge(&mut self, other: Self) {
        self.add(other.sum);
        self.add(other.correction);
    }

    #[inline]
    pub(crate) fn total(self) -> f64 {
        self.sum + self.correction
    }
}

/// Fixed-size ordered partials merged with Neumaier compensation.
///
/// The partial length is independent of decoder chunking. Common streaming
/// analysis therefore performs one ordinary addition per frame while the
/// compensated outer reduction bounds error over arbitrarily long inputs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct BlockCompensatedSum {
    completed: CompensatedSum,
    partial: f64,
    partial_values: u16,
}

impl BlockCompensatedSum {
    pub(crate) const VALUES_PER_PARTIAL: u16 = 1_024;

    pub(crate) const fn new() -> Self {
        Self {
            completed: CompensatedSum::new(),
            partial: 0.0,
            partial_values: 0,
        }
    }

    /// Add one value to the chunk-boundary-independent fast reduction.
    #[inline(always)]
    pub(crate) fn add_ordered(&mut self, value: f64) {
        self.partial += value;
        self.partial_values += 1;
        if self.partial_values == Self::VALUES_PER_PARTIAL {
            self.completed.add(self.partial);
            self.partial = 0.0;
            self.partial_values = 0;
        }
    }

    /// Add one value to two reductions on an absolute one-based frame grid.
    ///
    /// The two independent floating-point reductions retain exactly the same
    /// arithmetic and merge order as two [`Self::add_ordered`] calls. The
    /// caller-provided ordinal is the counter for this dedicated fixed-layout
    /// path, avoiding duplicate loop-carried state in both accumulators.
    ///
    /// An accumulator advanced through this method must use it exclusively;
    /// `value_ordinal` starts at one and increases by one for every call.
    #[inline(always)]
    pub(crate) fn add_ordered_frame_pair(
        &mut self,
        other: &mut Self,
        value: f64,
        other_value: f64,
        value_ordinal: usize,
    ) -> bool {
        debug_assert_eq!(self.partial_values, 0);
        debug_assert_eq!(other.partial_values, 0);
        self.partial += value;
        other.partial += other_value;
        if value_ordinal.is_multiple_of(usize::from(Self::VALUES_PER_PARTIAL)) {
            self.completed.add(self.partial);
            other.completed.add(other.partial);
            self.partial = 0.0;
            other.partial = 0.0;
            true
        } else {
            false
        }
    }

    /// Add one value directly to the strict scalar reduction.
    #[inline]
    pub(crate) fn add_exact(&mut self, value: f64) {
        debug_assert_eq!(self.partial_values, 0);
        self.completed.add(value);
    }

    #[inline]
    pub(crate) fn total(self) -> f64 {
        let mut completed = self.completed;
        completed.add(self.partial);
        completed.total()
    }
}

impl FromIterator<f64> for CompensatedSum {
    fn from_iter<T: IntoIterator<Item = f64>>(iter: T) -> Self {
        let mut sum = Self::new();
        for value in iter {
            sum.add(value);
        }
        sum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_terms_lost_by_ordinary_addition() {
        let ordinary = 1.0e16 + 1.0 - 1.0e16;
        let compensated = [1.0e16, 1.0, -1.0e16]
            .into_iter()
            .collect::<CompensatedSum>()
            .total();
        assert_eq!(ordinary, 0.0);
        assert_eq!(compensated, 1.0);
    }

    #[test]
    fn fixed_order_merge_preserves_partial_corrections() {
        let left = [1.0e16, 1.0].into_iter().collect::<CompensatedSum>();
        let right = [-1.0e16, 2.0].into_iter().collect::<CompensatedSum>();
        let mut combined = CompensatedSum::new();
        combined.merge(left);
        combined.merge(right);
        assert_eq!(combined.total(), 3.0);
    }

    #[test]
    fn blocked_sum_is_chunk_boundary_independent_and_retains_small_terms() {
        let values = (0_usize..8_193)
            .map(|index| {
                if index.is_multiple_of(1_024) {
                    1.0
                } else {
                    f64::EPSILON
                }
            })
            .collect::<Vec<_>>();
        let accumulate = |chunks: &[usize]| {
            let mut sum = BlockCompensatedSum::new();
            let mut start = 0;
            for &length in chunks {
                for &value in &values[start..start + length] {
                    sum.add_ordered(value);
                }
                start += length;
            }
            assert_eq!(start, values.len());
            sum.total()
        };
        let whole = accumulate(&[values.len()]);
        let split = accumulate(&[17, 2_031, 5, 4_096, 2_044]);
        assert_eq!(whole.to_bits(), split.to_bits());
        assert!(whole > 9.0);
    }

    #[test]
    fn paired_blocked_sums_match_independent_reductions_bit_for_bit() {
        let values = (0_usize..8_193)
            .map(|index| {
                let left = ((index.wrapping_mul(97) % 1_009) + 1) as f64 / 1_009.0;
                let right = ((index.wrapping_mul(193) % 2_003) + 1) as f64 / 2_003.0;
                (left, right)
            })
            .collect::<Vec<_>>();
        let mut expected_left = BlockCompensatedSum::new();
        let mut expected_right = BlockCompensatedSum::new();
        let mut paired_left = BlockCompensatedSum::new();
        let mut paired_right = BlockCompensatedSum::new();
        for (index, &(left, right)) in values.iter().enumerate() {
            expected_left.add_ordered(left);
            expected_right.add_ordered(right);
            let completed =
                paired_left.add_ordered_frame_pair(&mut paired_right, left, right, index + 1);
            assert_eq!(completed, (index + 1).is_multiple_of(1_024));
        }
        assert_eq!(paired_left.completed, expected_left.completed);
        assert_eq!(
            paired_left.partial.to_bits(),
            expected_left.partial.to_bits()
        );
        assert_eq!(paired_right.completed, expected_right.completed);
        assert_eq!(
            paired_right.partial.to_bits(),
            expected_right.partial.to_bits()
        );
        assert_eq!(paired_left.partial_values, 0);
        assert_eq!(paired_right.partial_values, 0);
        assert_eq!(
            paired_left.total().to_bits(),
            expected_left.total().to_bits()
        );
        assert_eq!(
            paired_right.total().to_bits(),
            expected_right.total().to_bits()
        );
    }
}
