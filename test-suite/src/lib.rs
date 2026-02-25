//! A simple library with utility functions for testing.

/// Adds two numbers together.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

/// Multiplies two numbers together.
pub fn multiply(a: i32, b: i32) -> i32 {
    a * b
}

/// Computes the factorial of a number (iteratively).
pub fn factorial(n: u64) -> u64 {
    if n == 0 {
        return 1;
    }
    let mut result = 1u64;
    for i in 1..=n {
        result = result.saturating_mul(i);
    }
    result
}

/// Computes the nth Fibonacci number (iteratively).
pub fn fibonacci(n: u32) -> u64 {
    if n == 0 {
        return 0;
    }
    if n == 1 {
        return 1;
    }
    let mut a = 0u64;
    let mut b = 1u64;
    for _ in 2..=n {
        let temp = a + b;
        a = b;
        b = temp;
    }
    b
}

/// Reverses a string.
pub fn reverse_string(s: &str) -> String {
    s.chars().rev().collect()
}

/// Checks if a string is a palindrome.
pub fn is_palindrome(s: &str) -> bool {
    let cleaned: String = s.chars().filter(|c| c.is_alphanumeric()).collect();
    let lower = cleaned.to_lowercase();
    lower == lower.chars().rev().collect::<String>()
}

/// Computes the sum of a slice of integers.
pub fn sum_slice(numbers: &[i32]) -> i32 {
    numbers.iter().sum()
}

/// Finds the maximum value in a slice, returns None if empty.
pub fn max_in_slice(numbers: &[i32]) -> Option<i32> {
    numbers.iter().copied().max()
}

/// Checks if a number is prime.
pub fn is_prime(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    if n == 2 {
        return true;
    }
    if n % 2 == 0 {
        return false;
    }
    let sqrt = (n as f64).sqrt() as u64;
    for i in (3..=sqrt).step_by(2) {
        if n % i == 0 {
            return false;
        }
    }
    true
}

/// Computes the greatest common divisor using Euclidean algorithm.
pub fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let temp = b;
        b = a % b;
        a = temp;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_basic() {
        assert_eq!(add(2, 3), 5);
    }

    #[test]
    fn test_multiply_basic() {
        assert_eq!(multiply(4, 5), 20);
    }

    #[test]
    fn test_factorial_basic() {
        assert_eq!(factorial(5), 120);
    }
}
