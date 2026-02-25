//! Fast tests that complete in 100-500ms each.

use rust_tests::{add, factorial, fibonacci, is_palindrome, multiply};
use std::thread::sleep;
use std::time::Duration;

#[test]
fn test_addition_operations() {
    // Simulate some work (100ms)
    sleep(Duration::from_millis(100));

    // Do actual computations
    let mut sum = 0i32;
    for i in 0..10000 {
        sum = sum.wrapping_add(add(i, i * 2));
    }

    assert_eq!(add(10, 20), 30);
    assert_eq!(add(-5, 5), 0);
    assert_eq!(add(0, 0), 0);
    assert!(sum != 0, "Sum should be non-zero");
}

#[test]
fn test_multiplication_chain() {
    // Simulate some work (200ms)
    sleep(Duration::from_millis(200));

    // Chain of multiplications
    let result = multiply(multiply(multiply(2, 3), 4), 5);
    assert_eq!(result, 120);

    // More multiplication tests
    assert_eq!(multiply(0, 100), 0);
    assert_eq!(multiply(-3, 4), -12);
    assert_eq!(multiply(-2, -2), 4);
}

#[test]
fn test_fibonacci_sequence() {
    // Simulate some work (300ms)
    sleep(Duration::from_millis(300));

    // Verify Fibonacci sequence
    assert_eq!(fibonacci(0), 0);
    assert_eq!(fibonacci(1), 1);
    assert_eq!(fibonacci(2), 1);
    assert_eq!(fibonacci(10), 55);
    assert_eq!(fibonacci(20), 6765);

    // Compute several Fibonacci numbers
    let fib_sum: u64 = (0..30).map(fibonacci).sum();
    assert!(fib_sum > 0, "Fibonacci sum should be positive");
}

#[test]
fn test_factorial_values() {
    // Simulate some work (400ms)
    sleep(Duration::from_millis(400));

    // Verify factorials
    assert_eq!(factorial(0), 1);
    assert_eq!(factorial(1), 1);
    assert_eq!(factorial(5), 120);
    assert_eq!(factorial(10), 3628800);

    // Compute factorial chain
    let mut product = 1u64;
    for i in 1..=10 {
        product = product.wrapping_mul(factorial(i));
    }
    assert!(product > 0);
}

#[test]
fn test_palindrome_detection() {
    // Simulate some work (500ms)
    sleep(Duration::from_millis(500));

    // Basic palindrome tests
    assert!(is_palindrome("racecar"));
    assert!(is_palindrome("A man a plan a canal Panama"));
    assert!(is_palindrome("Was it a car or a cat I saw"));
    assert!(!is_palindrome("hello world"));
    assert!(is_palindrome(""));
    assert!(is_palindrome("a"));

    // Test many strings
    let test_strings = vec!["level", "noon", "civic", "radar", "refer"];
    for s in test_strings {
        assert!(is_palindrome(s), "{} should be a palindrome", s);
    }
}
