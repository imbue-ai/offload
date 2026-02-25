//! Flaky tests that randomly fail to test retry mechanisms.

use rand::Rng;
use rust_tests::{add, is_prime, multiply};
use std::thread::sleep;
use std::time::Duration;

#[test]
fn test_flaky_random_calculation() {
    // Some work first
    sleep(Duration::from_millis(500));

    // Do actual computations
    let mut sum = 0i32;
    for i in 0..5000 {
        sum = sum.wrapping_add(add(i, multiply(i, 2)));
    }
    assert!(sum != 0);

    // Find some primes
    let prime_count = (2..500).filter(|&n| is_prime(n)).count();
    assert_eq!(prime_count, 95);

    // Random failure - approximately 40% of the time
    let mut rng = rand::thread_rng();
    let random_value: f64 = rng.gen();

    if random_value < 0.4 {
        panic!(
            "Flaky test failed! Random value {} was below threshold 0.4. \
             This simulates a non-deterministic failure in test infrastructure.",
            random_value
        );
    }

    // If we get here, the test passes
    assert!(random_value >= 0.4);
}

#[test]
fn test_flaky_timing_sensitive() {
    // Simulate timing-sensitive operation
    sleep(Duration::from_millis(800));

    // Do some string work
    let mut strings = Vec::new();
    for i in 0..1000 {
        strings.push(format!("test_item_{:05}", i));
    }

    // Verify strings were created correctly
    assert_eq!(strings.len(), 1000);
    assert_eq!(strings[0], "test_item_00000");
    assert_eq!(strings[999], "test_item_00999");

    // Compute hash-like values
    let hash_sum: usize = strings.iter().map(|s| s.len()).sum();
    assert_eq!(hash_sum, 15000); // Each string is 15 chars

    // Random failure - approximately 35% of the time
    let mut rng = rand::thread_rng();
    let random_value: f64 = rng.gen();

    if random_value < 0.35 {
        panic!(
            "Timing-sensitive test failed! Simulated race condition detected. \
             Random value: {:.4}. This represents a flaky timing issue.",
            random_value
        );
    }

    // Additional verification
    let sorted: Vec<_> = strings.iter().cloned().collect();
    assert!(sorted.windows(2).all(|w| w[0] <= w[1]));
}
