//! Mathematical utilities for no_std environment

/// Calculate integer square root using binary search
/// Returns the largest integer whose square is less than or equal to n
pub fn isqrt(n: u64) -> u64 {
    if n < 2 {
        return n;
    }

    // Binary search for the square root
    let mut left = 1u64;
    let mut right = n;
    let mut result = 0u64;

    while left <= right {
        let mid = left + (right - left) / 2;

        // Check if mid * mid == n, being careful about overflow
        match mid.checked_mul(mid) {
            Some(square) => {
                if square == n {
                    return mid;
                } else if square < n {
                    result = mid;
                    left = mid + 1;
                } else if mid > 0 {
                    right = mid - 1;
                } else {
                    break;
                }
            }
            None => {
                // Overflow, mid is too large
                if mid > 0 {
                    right = mid - 1;
                } else {
                    break;
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_isqrt() {
        assert_eq!(isqrt(0), 0);
        assert_eq!(isqrt(1), 1);
        assert_eq!(isqrt(4), 2);
        assert_eq!(isqrt(8), 2);
        assert_eq!(isqrt(9), 3);
        assert_eq!(isqrt(15), 3);
        assert_eq!(isqrt(16), 4);
        assert_eq!(isqrt(100), 10);
        assert_eq!(isqrt(99), 9);
        assert_eq!(isqrt(101), 10);
    }

    #[test]
    fn test_abs_diff() {
        assert_eq!(abs_diff(10, 5), 5);
        assert_eq!(abs_diff(5, 10), 5);
        assert_eq!(abs_diff(-5, 10), 15);
        assert_eq!(abs_diff(10, -5), 15);
        assert_eq!(abs_diff(-10, -5), 5);
    }
}
