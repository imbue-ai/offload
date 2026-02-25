//! Slow tests that complete in 5-10 seconds each.

use rust_tests::{fibonacci, gcd, is_prime, sum_slice};
use std::thread::sleep;
use std::time::Duration;

#[test]
fn test_extensive_prime_computation() {
    // Simulate heavy work (5 seconds)
    sleep(Duration::from_secs(5));

    // Find all primes up to 10000
    let primes: Vec<u64> = (2..10000).filter(|&n| is_prime(n)).collect();

    // There should be 1229 primes below 10000
    assert_eq!(primes.len(), 1229);

    // Verify some specific primes
    assert!(primes.contains(&2));
    assert!(primes.contains(&9973)); // Largest prime below 10000

    // Verify twin primes exist
    let twin_primes: Vec<_> = primes
        .windows(2)
        .filter(|w| w[1] - w[0] == 2)
        .collect();
    assert!(!twin_primes.is_empty());

    // Sum of first 100 primes
    let first_100_sum: u64 = primes.iter().take(100).sum();
    assert_eq!(first_100_sum, 24133);
}

#[test]
fn test_complex_gcd_matrix() {
    // Simulate heavy work (7 seconds)
    sleep(Duration::from_secs(7));

    // Build a GCD matrix for numbers 1-100
    let mut gcd_matrix = vec![vec![0u64; 100]; 100];
    for i in 0..100 {
        for j in 0..100 {
            gcd_matrix[i][j] = gcd((i + 1) as u64, (j + 1) as u64);
        }
    }

    // Verify diagonal is identity (gcd(n,n) = n)
    for i in 0..100 {
        assert_eq!(gcd_matrix[i][i], (i + 1) as u64);
    }

    // Verify symmetry (gcd(a,b) = gcd(b,a))
    for i in 0..100 {
        for j in 0..100 {
            assert_eq!(gcd_matrix[i][j], gcd_matrix[j][i]);
        }
    }

    // Verify gcd(1, n) = 1
    for j in 0..100 {
        assert_eq!(gcd_matrix[0][j], 1);
    }

    // Count coprime pairs
    let coprime_count: usize = gcd_matrix
        .iter()
        .flat_map(|row| row.iter())
        .filter(|&&g| g == 1)
        .count();
    assert!(coprime_count > 5000); // Most pairs should be coprime
}

#[test]
fn test_fibonacci_performance_analysis() {
    // Simulate heavy work (10 seconds)
    sleep(Duration::from_secs(10));

    // Compute Fibonacci numbers up to 70
    let fibs: Vec<u64> = (0..=70).map(fibonacci).collect();

    // Verify the sequence
    assert_eq!(fibs[0], 0);
    assert_eq!(fibs[1], 1);
    assert_eq!(fibs[50], 12586269025);
    assert_eq!(fibs[70], 190392490709135);

    // Verify the golden ratio approximation
    // F(n)/F(n-1) approaches phi (1.618...)
    let ratio = fibs[70] as f64 / fibs[69] as f64;
    let phi = (1.0 + 5.0_f64.sqrt()) / 2.0;
    assert!((ratio - phi).abs() < 0.0000001);

    // Analyze sum of odd-indexed Fibonacci numbers
    let odd_sum: u64 = fibs.iter().enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, &f)| f)
        .take(30)
        .sum();
    assert!(odd_sum > 0);

    // Test slice sum with Fibonacci values
    let fib_i32: Vec<i32> = fibs.iter().take(20).map(|&f| f as i32).collect();
    let sum = sum_slice(&fib_i32);
    assert_eq!(sum, 10945); // Sum of first 20 Fibonacci numbers
}
