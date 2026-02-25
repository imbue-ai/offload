//! Concurrent tests - 6 tests using threads for parallel operations.

use rust_tests::{fibonacci, is_prime, factorial};
use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};
use std::thread;
use std::time::Duration;

#[test]
fn test_parallel_prime_counting() {
    // Duration: ~1s
    thread::sleep(Duration::from_secs(1));

    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    // Spawn 4 threads to count primes in different ranges
    let ranges = [(2, 2500), (2500, 5000), (5000, 7500), (7500, 10000)];

    for (start, end) in ranges {
        let counter = Arc::clone(&counter);
        let handle = thread::spawn(move || {
            let count = (start..end).filter(|&n| is_prime(n)).count();
            counter.fetch_add(count, Ordering::SeqCst);
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // Total primes below 10000 = 1229
    assert_eq!(counter.load(Ordering::SeqCst), 1229);
}

#[test]
fn test_parallel_fibonacci_computation() {
    // Duration: ~1.5s
    thread::sleep(Duration::from_millis(1500));

    let results = Arc::new(Mutex::new(Vec::new()));
    let mut handles = vec![];

    // Compute Fibonacci numbers in parallel
    for chunk_start in (0..80).step_by(20) {
        let results = Arc::clone(&results);
        let handle = thread::spawn(move || {
            let chunk_results: Vec<(u32, u64)> = (chunk_start..chunk_start + 20)
                .map(|n| (n, fibonacci(n)))
                .collect();

            let mut results = results.lock().unwrap();
            results.extend(chunk_results);
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    let results = results.lock().unwrap();
    assert_eq!(results.len(), 80);

    // Sort and verify some values
    let mut sorted: Vec<_> = results.clone();
    sorted.sort_by_key(|&(n, _)| n);

    assert_eq!(sorted[0], (0, 0));
    assert_eq!(sorted[1], (1, 1));
    assert_eq!(sorted[10], (10, 55));
    assert_eq!(sorted[20], (20, 6765));
}

#[test]
fn test_parallel_factorial_computation() {
    // Duration: ~2s
    thread::sleep(Duration::from_secs(2));

    let results = Arc::new(Mutex::new(vec![0u64; 21]));
    let mut handles = vec![];

    // Compute factorials 0-20 in parallel using 3 threads
    let ranges = [(0, 7), (7, 14), (14, 21)];

    for (start, end) in ranges {
        let results = Arc::clone(&results);
        let handle = thread::spawn(move || {
            for n in start..end {
                let fact = factorial(n as u64);
                let mut results = results.lock().unwrap();
                results[n] = fact;
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    let results = results.lock().unwrap();

    // Verify results
    assert_eq!(results[0], 1);
    assert_eq!(results[1], 1);
    assert_eq!(results[5], 120);
    assert_eq!(results[10], 3628800);
    assert_eq!(results[20], 2432902008176640000);

    // Verify factorial property
    for i in 1..21 {
        assert_eq!(results[i], (i as u64) * results[i - 1]);
    }
}

#[test]
fn test_parallel_sum_reduction() {
    // Duration: ~2.5s
    thread::sleep(Duration::from_millis(2500));

    let partial_sums = Arc::new(Mutex::new(Vec::new()));
    let mut handles = vec![];

    // Sum numbers 1 to 100000 using 5 threads
    let chunk_size = 20000;

    for i in 0..5 {
        let partial_sums = Arc::clone(&partial_sums);
        let start = i * chunk_size + 1;
        let end = (i + 1) * chunk_size;

        let handle = thread::spawn(move || {
            // Do actual computation
            let sum: u64 = (start..=end).sum();

            // Also do some extra work
            let primes: Vec<u64> = (start..=std::cmp::min(end, start + 1000))
                .filter(|&n| is_prime(n))
                .collect();

            let mut sums = partial_sums.lock().unwrap();
            sums.push((i, sum, primes.len()));
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    let sums = partial_sums.lock().unwrap();

    // Calculate total sum
    let total: u64 = sums.iter().map(|(_, s, _)| *s).sum();

    // Sum of 1 to 100000 = 100000 * 100001 / 2 = 5000050000
    assert_eq!(total, 5000050000);

    // Verify we computed primes in each thread
    assert!(sums.iter().all(|(_, _, prime_count)| *prime_count > 0));
}

#[test]
fn test_thread_local_computation() {
    // Duration: ~3s
    thread::sleep(Duration::from_secs(3));

    let results = Arc::new(Mutex::new(Vec::new()));
    let mut handles = vec![];

    for thread_id in 0..4 {
        let results = Arc::clone(&results);

        let handle = thread::spawn(move || {
            // Each thread does independent work
            let mut local_results = Vec::new();

            // Compute Fibonacci for a range
            let start = thread_id * 10;
            let end = start + 10;
            for n in start..end {
                local_results.push(("fib", n, fibonacci(n as u32)));
            }

            // Find primes in a range
            let prime_start = thread_id * 500;
            let prime_end = prime_start + 500;
            let prime_count = (prime_start..prime_end).filter(|&n| is_prime(n)).count();
            local_results.push(("primes", thread_id as u64, prime_count as u64));

            // Compute factorials
            for n in 0..5 {
                local_results.push(("fact", n + thread_id * 5, factorial((n + thread_id * 5) as u64)));
            }

            // Submit results
            let mut results = results.lock().unwrap();
            results.extend(local_results);
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    let results = results.lock().unwrap();

    // Count results by type
    let fib_count = results.iter().filter(|(t, _, _)| *t == "fib").count();
    let prime_count = results.iter().filter(|(t, _, _)| *t == "primes").count();
    let fact_count = results.iter().filter(|(t, _, _)| *t == "fact").count();

    assert_eq!(fib_count, 40); // 4 threads * 10 fibs each
    assert_eq!(prime_count, 4); // 1 prime count per thread
    assert_eq!(fact_count, 20); // 4 threads * 5 facts each
}

#[test]
fn test_parallel_matrix_operations() {
    // Duration: ~4s
    thread::sleep(Duration::from_secs(4));

    // Matrix multiplication using threads
    let n = 50;
    let a: Vec<Vec<i32>> = (0..n).map(|i| (0..n).map(|j| ((i * j) % 10) as i32).collect()).collect();
    let b: Vec<Vec<i32>> = (0..n).map(|i| (0..n).map(|j| ((i + j) % 10) as i32).collect()).collect();

    let a = Arc::new(a);
    let b = Arc::new(b);
    let result = Arc::new(Mutex::new(vec![vec![0i32; n]; n]));

    let mut handles = vec![];

    // Each thread computes a portion of rows
    let rows_per_thread = n / 5;
    for t in 0..5 {
        let a = Arc::clone(&a);
        let b = Arc::clone(&b);
        let result = Arc::clone(&result);

        let handle = thread::spawn(move || {
            let start_row = t * rows_per_thread;
            let end_row = if t == 4 { n } else { (t + 1) * rows_per_thread };

            let mut local_result = vec![vec![0i32; n]; end_row - start_row];

            for i in start_row..end_row {
                for j in 0..n {
                    let mut sum = 0;
                    for k in 0..n {
                        sum += a[i][k] * b[k][j];
                    }
                    local_result[i - start_row][j] = sum;
                }
            }

            // Write to shared result
            let mut result = result.lock().unwrap();
            for (i, row) in local_result.into_iter().enumerate() {
                result[start_row + i] = row;
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    let result = result.lock().unwrap();

    // Verify matrix is complete
    assert_eq!(result.len(), n);
    assert!(result.iter().all(|row| row.len() == n));

    // Verify a few elements manually
    // C[0][0] = sum of A[0][k] * B[k][0] for k=0..n
    // A[0][k] = 0 for all k, so C[0][0] = 0
    assert_eq!(result[0][0], 0);

    // C[1][1] should be computable
    let expected_1_1: i32 = (0..n).map(|k| ((1 * k) % 10) as i32 * ((k + 1) % 10) as i32).sum();
    assert_eq!(result[1][1], expected_1_1);
}
