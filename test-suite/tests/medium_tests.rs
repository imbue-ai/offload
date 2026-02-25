//! Medium tests that complete in 1-3 seconds each.

use rust_tests::{factorial, fibonacci, gcd, is_prime, max_in_slice, reverse_string, sum_slice};
use std::thread::sleep;
use std::time::Duration;

#[test]
fn test_prime_number_search() {
    // Simulate work (1 second)
    sleep(Duration::from_secs(1));

    // Find all primes up to 1000
    let primes: Vec<u64> = (2..1000).filter(|&n| is_prime(n)).collect();

    // Verify known primes
    assert!(is_prime(2));
    assert!(is_prime(17));
    assert!(is_prime(97));
    assert!(is_prime(997));

    // Verify non-primes
    assert!(!is_prime(0));
    assert!(!is_prime(1));
    assert!(!is_prime(4));
    assert!(!is_prime(100));

    // There should be 168 primes below 1000
    assert_eq!(primes.len(), 168);
}

#[test]
fn test_string_manipulations() {
    // Simulate work (1.5 seconds)
    sleep(Duration::from_millis(1500));

    // Basic reverse tests
    assert_eq!(reverse_string("hello"), "olleh");
    assert_eq!(reverse_string(""), "");
    assert_eq!(reverse_string("a"), "a");

    // Build and reverse many strings
    let mut results = Vec::new();
    for i in 0..1000 {
        let s = format!("test_string_{:04}", i);
        let reversed = reverse_string(&s);
        results.push(reversed);
    }

    // Verify first and last
    assert_eq!(results[0], "0000_gnirts_tset");
    assert_eq!(results[999], "9990_gnirts_tset");

    // Unicode handling
    assert_eq!(reverse_string("hello 世界"), "界世 olleh");
}

#[test]
fn test_gcd_calculations() {
    // Simulate work (2 seconds)
    sleep(Duration::from_secs(2));

    // Basic GCD tests
    assert_eq!(gcd(48, 18), 6);
    assert_eq!(gcd(100, 25), 25);
    assert_eq!(gcd(17, 13), 1); // Coprime
    assert_eq!(gcd(0, 5), 5);
    assert_eq!(gcd(5, 0), 5);

    // Compute many GCDs
    let mut total_gcd = 0u64;
    for i in 1..500 {
        for j in 1..100 {
            total_gcd = total_gcd.wrapping_add(gcd(i * 7, j * 11));
        }
    }
    assert!(total_gcd > 0);

    // GCD with large numbers
    assert_eq!(gcd(1000000007, 1000000009), 1); // Both are prime
}

#[test]
fn test_slice_operations() {
    // Simulate work (2.5 seconds)
    sleep(Duration::from_millis(2500));

    // Create large slices and test operations
    let large_slice: Vec<i32> = (1..=10000).collect();
    let sum = sum_slice(&large_slice);
    assert_eq!(sum, 50005000); // Sum of 1 to 10000

    let max = max_in_slice(&large_slice);
    assert_eq!(max, Some(10000));

    // Empty slice
    let empty: Vec<i32> = vec![];
    assert_eq!(sum_slice(&empty), 0);
    assert_eq!(max_in_slice(&empty), None);

    // Negative numbers
    let negatives = vec![-5, -2, -8, -1, -10];
    assert_eq!(sum_slice(&negatives), -26);
    assert_eq!(max_in_slice(&negatives), Some(-1));

    // Mixed numbers
    let mixed = vec![-100, 50, -25, 75, 0];
    assert_eq!(max_in_slice(&mixed), Some(75));
}

#[test]
fn test_fibonacci_and_factorial_combined() {
    // Simulate work (3 seconds)
    sleep(Duration::from_secs(3));

    // Compute many Fibonacci numbers
    let fibs: Vec<u64> = (0..40).map(fibonacci).collect();
    assert_eq!(fibs[0], 0);
    assert_eq!(fibs[1], 1);
    assert_eq!(fibs[39], 63245986);

    // Compute factorials
    let facts: Vec<u64> = (0..15).map(factorial).collect();
    assert_eq!(facts[0], 1);
    assert_eq!(facts[10], 3628800);

    // Verify Fibonacci property: F(n) = F(n-1) + F(n-2)
    for i in 2..40 {
        assert_eq!(fibs[i], fibs[i - 1] + fibs[i - 2]);
    }

    // Verify factorial property: n! = n * (n-1)!
    for i in 1..15 {
        assert_eq!(facts[i], (i as u64) * facts[i - 1]);
    }
}
