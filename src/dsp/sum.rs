//! Deterministic compensated floating-point accumulation.

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
}
