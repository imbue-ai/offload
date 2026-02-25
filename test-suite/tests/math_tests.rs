//! Math operation tests - 10 tests for mathematical computations.

use rust_tests::{factorial, fibonacci, gcd, is_prime};
use std::thread::sleep;
use std::time::Duration;

#[test]
fn test_factorial_large_numbers() {
    // Duration: ~100ms
    sleep(Duration::from_millis(100));

    // Compute factorials for numbers 0-20
    let factorials: Vec<u64> = (0..=20).map(factorial).collect();

    // Verify specific values
    assert_eq!(factorials[0], 1);
    assert_eq!(factorials[1], 1);
    assert_eq!(factorials[5], 120);
    assert_eq!(factorials[10], 3628800);
    assert_eq!(factorials[12], 479001600);
    assert_eq!(factorials[15], 1307674368000);
    assert_eq!(factorials[20], 2432902008176640000);

    // Verify property: n! = n * (n-1)!
    for n in 1..=20 {
        assert_eq!(factorials[n], (n as u64) * factorials[n - 1]);
    }
}

#[test]
fn test_factorial_sum_products() {
    // Duration: ~300ms
    sleep(Duration::from_millis(300));

    // Compute sum of factorials
    let sum: u64 = (0..=15).map(factorial).sum();
    assert_eq!(sum, 1_401_602_636_314);

    // Compute alternating sum: 0! - 1! + 2! - 3! + ...
    let alternating_sum: i64 = (0..=10)
        .map(|n| {
            let f = factorial(n) as i64;
            if n % 2 == 0 { f } else { -f }
        })
        .sum();
    assert_eq!(alternating_sum, 3_301_820);
}

#[test]
fn test_fibonacci_properties() {
    // Duration: ~500ms
    sleep(Duration::from_millis(500));

    // Compute first 50 Fibonacci numbers
    let fibs: Vec<u64> = (0..50).map(fibonacci).collect();

    // Verify identity: F(2n) = F(n) * (2*F(n+1) - F(n))
    for n in 1..20 {
        let f_2n = fibs[2 * n];
        let computed = fibs[n] * (2 * fibs[n + 1] - fibs[n]);
        assert_eq!(f_2n, computed, "Identity failed for n={}", n);
    }

    // Verify Cassini's identity: F(n-1)*F(n+1) - F(n)^2 = (-1)^n
    for n in 2..40 {
        let left = (fibs[n - 1] as i64) * (fibs[n + 1] as i64) - (fibs[n] as i64).pow(2);
        // (-1)^n: positive for even n, negative for odd n
        let right = if n % 2 == 0 { 1 } else { -1 };
        assert_eq!(left, right, "Cassini's identity failed for n={}", n);
    }
}

#[test]
fn test_fibonacci_sum_identities() {
    // Duration: ~800ms
    sleep(Duration::from_millis(800));

    let fibs: Vec<u64> = (0..60).map(fibonacci).collect();

    // Sum of first n Fibonacci numbers = F(n+2) - 1
    for n in 1..40 {
        let sum: u64 = fibs[0..=n].iter().sum();
        assert_eq!(sum, fibs[n + 2] - 1, "Sum identity failed for n={}", n);
    }

    // Sum of first n even-indexed Fibonacci numbers
    // F(0) + F(2) + F(4) + ... + F(2n) = F(2n+1) - 1
    for n in 1..20 {
        let sum: u64 = (0..=n).map(|i| fibs[2 * i]).sum();
        assert_eq!(sum, fibs[2 * n + 1] - 1, "Even sum identity failed for n={}", n);
    }
}

#[test]
fn test_prime_sieve_verification() {
    // Duration: ~1s
    sleep(Duration::from_secs(1));

    // Find all primes up to 5000
    let primes: Vec<u64> = (2..5000).filter(|&n| is_prime(n)).collect();

    // Verify prime count (pi(5000) = 669)
    assert_eq!(primes.len(), 669);

    // Verify first 10 primes
    let first_10 = vec![2, 3, 5, 7, 11, 13, 17, 19, 23, 29];
    assert_eq!(&primes[..10], &first_10);

    // Verify Mersenne primes in range
    let mersenne_primes = [3, 7, 31, 127];
    for mp in mersenne_primes {
        assert!(primes.contains(&mp), "Mersenne prime {} not found", mp);
    }

    // Verify Sophie Germain primes (p where 2p+1 is also prime)
    let sophie_germain: Vec<u64> = primes
        .iter()
        .filter(|&&p| is_prime(2 * p + 1))
        .copied()
        .take(10)
        .collect();
    assert_eq!(sophie_germain, vec![2, 3, 5, 11, 23, 29, 41, 53, 83, 89]);
}

#[test]
fn test_prime_gaps_analysis() {
    // Duration: ~1.2s
    sleep(Duration::from_millis(1200));

    let primes: Vec<u64> = (2..10000).filter(|&n| is_prime(n)).collect();

    // Analyze prime gaps
    let gaps: Vec<u64> = primes.windows(2).map(|w| w[1] - w[0]).collect();

    // Minimum gap (after 2,3) is 2
    assert!(gaps.iter().skip(1).all(|&g| g >= 2));

    // Count twin primes (gap of 2)
    let twin_count = gaps.iter().filter(|&&g| g == 2).count();
    assert!(twin_count > 200, "Expected more than 200 twin prime pairs");

    // Find maximum gap below 10000
    let max_gap = *gaps.iter().max().unwrap();
    assert!(max_gap <= 36, "Maximum gap should be <= 36 for primes < 10000");
}

#[test]
fn test_gcd_properties() {
    // Duration: ~1.5s
    sleep(Duration::from_millis(1500));

    // Test associativity: gcd(gcd(a, b), c) = gcd(a, gcd(b, c))
    for a in 1..50 {
        for b in 1..50 {
            for c in 1..20 {
                let left = gcd(gcd(a, b), c);
                let right = gcd(a, gcd(b, c));
                assert_eq!(left, right, "Associativity failed for ({}, {}, {})", a, b, c);
            }
        }
    }

    // Test distributivity: gcd(a*c, b*c) = c * gcd(a, b)
    for a in 1..30 {
        for b in 1..30 {
            for c in 1..10 {
                let left = gcd(a * c, b * c);
                let right = c * gcd(a, b);
                assert_eq!(left, right, "Distributivity failed for ({}, {}, {})", a, b, c);
            }
        }
    }
}

#[test]
fn test_gcd_lcm_relationship() {
    // Duration: ~1.8s
    sleep(Duration::from_millis(1800));

    // lcm(a, b) = a * b / gcd(a, b)
    fn lcm(a: u64, b: u64) -> u64 {
        if a == 0 || b == 0 {
            return 0;
        }
        a / gcd(a, b) * b
    }

    // Verify lcm properties
    for a in 1..100 {
        for b in 1..100 {
            let g = gcd(a, b);
            let l = lcm(a, b);

            // gcd * lcm = a * b
            assert_eq!(g * l, a * b, "gcd*lcm != a*b for ({}, {})", a, b);

            // a divides lcm and b divides lcm
            assert_eq!(l % a, 0, "a does not divide lcm({}, {})", a, b);
            assert_eq!(l % b, 0, "b does not divide lcm({}, {})", a, b);

            // gcd divides both a and b
            assert_eq!(a % g, 0, "gcd does not divide a for ({}, {})", a, b);
            assert_eq!(b % g, 0, "gcd does not divide b for ({}, {})", a, b);
        }
    }
}

#[test]
fn test_euler_totient_approximation() {
    // Duration: ~2s
    sleep(Duration::from_secs(2));

    // Compute Euler's totient function: phi(n) = count of k where gcd(k, n) = 1
    fn euler_totient(n: u64) -> u64 {
        (1..=n).filter(|&k| gcd(k, n) == 1).count() as u64
    }

    // Verify phi(p) = p - 1 for primes
    let primes: Vec<u64> = (2..100).filter(|&n| is_prime(n)).collect();
    for &p in &primes {
        assert_eq!(euler_totient(p), p - 1, "phi(p) != p-1 for prime {}", p);
    }

    // Verify phi(p^k) = p^(k-1) * (p - 1) for prime powers
    for &p in &[2u64, 3, 5, 7] {
        for k in 1..=4 {
            let pk = p.pow(k);
            let expected = p.pow(k - 1) * (p - 1);
            assert_eq!(euler_totient(pk), expected, "phi({}) != {} for p={}, k={}", pk, expected, p, k);
        }
    }

    // Verify multiplicativity: phi(m*n) = phi(m) * phi(n) when gcd(m, n) = 1
    for m in 1..20 {
        for n in 1..20 {
            if gcd(m, n) == 1 {
                assert_eq!(
                    euler_totient(m * n),
                    euler_totient(m) * euler_totient(n),
                    "Multiplicativity failed for ({}, {})",
                    m,
                    n
                );
            }
        }
    }
}

#[test]
fn test_number_theory_combined() {
    // Duration: ~1.5s
    sleep(Duration::from_millis(1500));

    // Test relationship between Fibonacci and GCD
    // gcd(F(m), F(n)) = F(gcd(m, n))
    for m in 1..20 {
        for n in 1..20 {
            let left = gcd(fibonacci(m), fibonacci(n));
            let right = fibonacci(gcd(m as u64, n as u64) as u32);
            assert_eq!(left, right, "Fib GCD property failed for ({}, {})", m, n);
        }
    }

    // Prime Fibonacci numbers
    let fib_primes: Vec<u32> = (3..30)
        .filter(|&n| is_prime(fibonacci(n)))
        .collect();
    assert!(fib_primes.contains(&3)); // F(3) = 2
    assert!(fib_primes.contains(&4)); // F(4) = 3
    assert!(fib_primes.contains(&5)); // F(5) = 5
    assert!(fib_primes.contains(&7)); // F(7) = 13
    assert!(fib_primes.contains(&11)); // F(11) = 89
}
