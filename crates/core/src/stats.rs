//! Latency statistics (median, percentiles, standard deviation) shared by
//! the reporters and the runner's benchmark mode.

/// Calculate the sample standard deviation of a slice of `u128` values.
#[must_use]
pub fn calculate_standard_deviation(values: &[u128], mean: u128) -> u128 {
    calculate_variance(values, mean).isqrt()
}

/// Calculate the sample variance of a slice of `u128` values.
///
/// Returns 0 for slices shorter than two elements: the sample variance is
/// undefined for fewer than two samples, and the guard also avoids the `len -
/// 1` underflow on an empty slice.
fn calculate_variance(values: &[u128], mean: u128) -> u128 {
    if values.len() < 2 {
        return 0;
    }

    let deviation = values.iter().map(|v| v.abs_diff(mean).pow(2)).sum::<u128>();
    deviation
        .checked_div(values.len() as u128 - 1)
        .unwrap_or_default()
}

/// Calculate the median of a slice of `u128` values.
/// Requires a sorted slice containing at least 1 value.
#[must_use]
pub fn calculate_median(values: &[u128]) -> u128 {
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid])
            .checked_div(2)
            .unwrap_or_default()
    } else {
        values[mid]
    }
}

/// Calculate the P90 of a slice of `u128` values.
/// Requires a sorted slice containing at least 1 value.
#[must_use]
pub fn calculate_p90(values: &[u128]) -> u128 {
    let percentile = (values.len() - 1) as f64 * 0.9;
    values[percentile.floor() as usize]
}

/// Calculate the P99 of a slice of `u128` values.
/// Requires a sorted slice containing at least 1 value.
#[must_use]
pub fn calculate_p99(values: &[u128]) -> u128 {
    let percentile = (values.len() - 1) as f64 * 0.99;
    values[percentile.floor() as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- median ----------

    #[test]
    fn median_of_odd_count() {
        assert_eq!(calculate_median(&[1, 3, 9]), 3);
    }

    #[test]
    fn median_of_even_count_averages_middle_pair() {
        assert_eq!(calculate_median(&[1, 3, 5, 100]), 4);
    }

    #[test]
    fn median_of_single_value() {
        assert_eq!(calculate_median(&[42]), 42);
    }

    // ---------- percentiles ----------

    #[test]
    fn p90_of_ten_sorted_values() {
        let values: Vec<u128> = (1..=10).collect();
        // index = (10-1) * 0.9 = 8.1 -> floor 8 -> value 9
        assert_eq!(calculate_p90(&values), 9);
    }

    #[test]
    fn p99_of_hundred_sorted_values() {
        let values: Vec<u128> = (1..=100).collect();
        // index = 99 * 0.99 = 98.01 -> floor 98 -> value 99
        assert_eq!(calculate_p99(&values), 99);
    }

    #[test]
    fn percentiles_of_single_value() {
        let values = vec![7];
        assert_eq!(calculate_p90(&values), 7);
        assert_eq!(calculate_p99(&values), 7);
    }

    // ---------- standard deviation ----------

    #[test]
    fn std_deviation_of_identical_values_is_zero() {
        let values = vec![5, 5, 5, 5];
        assert_eq!(calculate_standard_deviation(&values, 5), 0);
    }

    #[test]
    fn std_deviation_handles_values_below_the_mean() {
        // mean of [2, 4, 6] = 4; 2 < 4 must not underflow u128.
        // sample variance = ((4-2)^2 + 0 + (6-4)^2) / (3-1) = 4 -> sqrt = 2
        let values = vec![2, 4, 6];
        assert_eq!(calculate_standard_deviation(&values, 4), 2);
    }

    #[test]
    fn std_deviation_of_single_value_is_zero() {
        let values = vec![10];
        assert_eq!(calculate_standard_deviation(&values, 10), 0);
    }

    #[test]
    fn std_deviation_of_empty_slice_is_zero() {
        // Must not underflow on `len - 1` when there are no samples.
        assert_eq!(calculate_standard_deviation(&[], 0), 0);
    }
}
