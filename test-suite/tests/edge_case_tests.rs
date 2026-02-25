//! Edge case tests - 8 tests for boundary conditions and edge cases.

use rust_tests::{add, multiply, factorial, fibonacci, gcd, is_prime, is_palindrome, reverse_string, sum_slice, max_in_slice};
use std::thread::sleep;
use std::time::Duration;

#[test]
fn test_integer_overflow_handling() {
    // Duration: ~100ms
    sleep(Duration::from_millis(100));

    // Test add with potential overflow (wrapping behavior)
    assert_eq!(add(i32::MAX, 0), i32::MAX);
    assert_eq!(add(i32::MIN, 0), i32::MIN);
    assert_eq!(add(0, i32::MAX), i32::MAX);
    assert_eq!(add(0, i32::MIN), i32::MIN);

    // Test multiply edge cases
    assert_eq!(multiply(0, i32::MAX), 0);
    assert_eq!(multiply(i32::MAX, 0), 0);
    assert_eq!(multiply(1, i32::MAX), i32::MAX);
    assert_eq!(multiply(i32::MAX, 1), i32::MAX);
    assert_eq!(multiply(-1, i32::MAX), -i32::MAX);
    assert_eq!(multiply(i32::MIN, 1), i32::MIN);

    // Multiplication that would overflow
    assert_eq!(multiply(2, 2), 4);
    assert_eq!(multiply(-2, -2), 4);
    assert_eq!(multiply(-2, 2), -4);
}

#[test]
fn test_factorial_edge_cases() {
    // Duration: ~300ms
    sleep(Duration::from_millis(300));

    // Zero factorial
    assert_eq!(factorial(0), 1);

    // One factorial
    assert_eq!(factorial(1), 1);

    // Small factorials
    assert_eq!(factorial(2), 2);
    assert_eq!(factorial(3), 6);

    // Large factorial (testing saturation)
    let large_fact = factorial(20);
    assert_eq!(large_fact, 2432902008176640000);

    // Very large factorial (should saturate)
    let very_large = factorial(100);
    assert!(very_large > 0); // Should not be zero due to saturation

    // Verify factorial never returns zero for any input
    for n in 0..=30 {
        assert!(factorial(n) > 0, "factorial({}) should be positive", n);
    }
}

#[test]
fn test_fibonacci_edge_cases() {
    // Duration: ~400ms
    sleep(Duration::from_millis(400));

    // Base cases
    assert_eq!(fibonacci(0), 0);
    assert_eq!(fibonacci(1), 1);
    assert_eq!(fibonacci(2), 1);

    // Small values
    assert_eq!(fibonacci(3), 2);
    assert_eq!(fibonacci(4), 3);
    assert_eq!(fibonacci(5), 5);

    // Larger values near u64 boundary
    assert_eq!(fibonacci(90), 2880067194370816120);
    assert_eq!(fibonacci(91), 4660046610375530309);
    assert_eq!(fibonacci(92), 7540113804746346429);

    // Verify Fibonacci property holds at boundaries
    for n in 2..=90 {
        let fib_n = fibonacci(n);
        let fib_n_1 = fibonacci(n - 1);
        let fib_n_2 = fibonacci(n - 2);
        assert_eq!(fib_n, fib_n_1 + fib_n_2, "Fibonacci property failed at n={}", n);
    }
}

#[test]
fn test_gcd_edge_cases() {
    // Duration: ~500ms
    sleep(Duration::from_millis(500));

    // GCD with zero
    assert_eq!(gcd(0, 5), 5);
    assert_eq!(gcd(5, 0), 5);
    assert_eq!(gcd(0, 0), 0);

    // GCD with one
    assert_eq!(gcd(1, 100), 1);
    assert_eq!(gcd(100, 1), 1);
    assert_eq!(gcd(1, 1), 1);

    // GCD with same number
    for n in 1..100 {
        assert_eq!(gcd(n, n), n, "gcd(n, n) should equal n");
    }

    // GCD with consecutive numbers (always 1)
    for n in 1..100 {
        assert_eq!(gcd(n, n + 1), 1, "Consecutive numbers should be coprime");
    }

    // GCD with very large numbers
    assert_eq!(gcd(u64::MAX, u64::MAX), u64::MAX);
    assert_eq!(gcd(u64::MAX, 1), 1);
    assert_eq!(gcd(1, u64::MAX), 1);

    // GCD with powers of 2
    assert_eq!(gcd(1024, 512), 512);
    assert_eq!(gcd(4096, 256), 256);
}

#[test]
fn test_prime_edge_cases() {
    // Duration: ~600ms
    sleep(Duration::from_millis(600));

    // Small numbers
    assert!(!is_prime(0));
    assert!(!is_prime(1));
    assert!(is_prime(2));
    assert!(is_prime(3));
    assert!(!is_prime(4));
    assert!(is_prime(5));

    // Perfect squares (non-prime)
    for n in 2..50 {
        assert!(!is_prime(n * n), "{} is a perfect square, not prime", n * n);
    }

    // Powers of 2 (only 2 is prime)
    assert!(is_prime(2));
    for exp in 2..30 {
        let power = 2u64.pow(exp);
        assert!(!is_prime(power), "2^{} = {} should not be prime", exp, power);
    }

    // Mersenne numbers (2^p - 1)
    // 2^2 - 1 = 3 (prime)
    // 2^3 - 1 = 7 (prime)
    // 2^5 - 1 = 31 (prime)
    // 2^7 - 1 = 127 (prime)
    assert!(is_prime(3));
    assert!(is_prime(7));
    assert!(is_prime(31));
    assert!(is_prime(127));

    // Fermat numbers F_n = 2^(2^n) + 1
    // F_0 = 3 (prime), F_1 = 5 (prime), F_2 = 17 (prime), F_3 = 257 (prime), F_4 = 65537 (prime)
    assert!(is_prime(3));
    assert!(is_prime(5));
    assert!(is_prime(17));
    assert!(is_prime(257));
    assert!(is_prime(65537));
}

#[test]
fn test_string_edge_cases() {
    // Duration: ~700ms
    sleep(Duration::from_millis(700));

    // Empty string
    assert_eq!(reverse_string(""), "");
    assert!(is_palindrome(""));

    // Single character
    assert_eq!(reverse_string("a"), "a");
    assert!(is_palindrome("a"));
    assert_eq!(reverse_string("Z"), "Z");

    // Two characters
    assert_eq!(reverse_string("ab"), "ba");
    assert!(is_palindrome("aa"));
    assert!(!is_palindrome("ab"));

    // Whitespace only
    assert_eq!(reverse_string("   "), "   ");
    assert!(is_palindrome("   ")); // All spaces are filtered out

    // Special characters
    assert_eq!(reverse_string("!@#"), "#@!");
    assert!(is_palindrome("!@#@!")); // Non-alphanumeric filtered

    // Numbers in strings
    assert_eq!(reverse_string("12321"), "12321");
    assert!(is_palindrome("12321"));

    // Very long palindrome
    let long_palindrome: String = (0..1000).map(|_| 'a').collect();
    assert!(is_palindrome(&long_palindrome));

    // Long non-palindrome
    let long_non_palindrome: String = (0..1000).map(|i| if i < 500 { 'a' } else { 'b' }).collect();
    assert!(!is_palindrome(&long_non_palindrome));
}

#[test]
fn test_slice_edge_cases() {
    // Duration: ~800ms
    sleep(Duration::from_millis(800));

    // Empty slice
    let empty: Vec<i32> = vec![];
    assert_eq!(sum_slice(&empty), 0);
    assert_eq!(max_in_slice(&empty), None);

    // Single element
    let single = vec![42];
    assert_eq!(sum_slice(&single), 42);
    assert_eq!(max_in_slice(&single), Some(42));

    // Two elements
    let two = vec![10, 20];
    assert_eq!(sum_slice(&two), 30);
    assert_eq!(max_in_slice(&two), Some(20));

    // All same elements
    let same = vec![5, 5, 5, 5, 5];
    assert_eq!(sum_slice(&same), 25);
    assert_eq!(max_in_slice(&same), Some(5));

    // All zeros
    let zeros = vec![0; 100];
    assert_eq!(sum_slice(&zeros), 0);
    assert_eq!(max_in_slice(&zeros), Some(0));

    // All negative
    let negatives = vec![-1, -2, -3, -4, -5];
    assert_eq!(sum_slice(&negatives), -15);
    assert_eq!(max_in_slice(&negatives), Some(-1));

    // Mixed with extremes
    let extremes = vec![i32::MIN, 0, i32::MAX];
    assert_eq!(max_in_slice(&extremes), Some(i32::MAX));

    // Large values that don't overflow
    let large_values = vec![1_000_000, 2_000_000, 3_000_000];
    assert_eq!(sum_slice(&large_values), 6_000_000);
    assert_eq!(max_in_slice(&large_values), Some(3_000_000));
}

#[test]
fn test_combined_edge_cases() {
    // Duration: ~1s
    sleep(Duration::from_secs(1));

    // Test combinations of edge case inputs

    // Fibonacci of prime indices
    let prime_fib_values: Vec<u64> = (2..50u32)
        .filter(|&n| is_prime(n as u64))
        .map(fibonacci)
        .collect();

    // All should be positive
    assert!(prime_fib_values.iter().all(|&v| v > 0));

    // GCD of consecutive Fibonacci numbers is always 1
    for n in 2..50 {
        let fib_n = fibonacci(n);
        let fib_n_1 = fibonacci(n - 1);
        assert_eq!(gcd(fib_n, fib_n_1), 1, "F({}) and F({}) should be coprime", n, n - 1);
    }

    // Factorial divides larger factorial
    for n in 1..=15u64 {
        for m in n..=15 {
            let fact_n = factorial(n);
            let fact_m = factorial(m);
            assert_eq!(fact_m % fact_n, 0, "{}! should divide {}!", n, m);
        }
    }

    // Prime factorials: n! + 1 is sometimes prime (Wilson's theorem related)
    // If p is prime, then (p-1)! + 1 is divisible by p
    for p in [5u64, 7, 11, 13] {
        if is_prime(p) {
            let fact = factorial(p - 1);
            let val = fact + 1;
            assert_eq!(val % p, 0, "Wilson's theorem: ({}-1)! + 1 should be divisible by {}", p, p);
        }
    }

    // Palindrome numbers that are also prime
    let palindrome_primes: Vec<u64> = (1..1000)
        .map(|n| n.to_string())
        .filter(|s| is_palindrome(s))
        .map(|s| s.parse::<u64>().unwrap())
        .filter(|&n| is_prime(n))
        .collect();

    // Should include 2, 3, 5, 7, 11, 101, 131, 151, 181, 191, etc.
    assert!(palindrome_primes.contains(&2));
    assert!(palindrome_primes.contains(&11));
    assert!(palindrome_primes.contains(&101));
    assert!(palindrome_primes.contains(&131));
}
