//! Algorithm tests - 8 tests for sorting and searching algorithms.

use rust_tests::{is_prime, gcd};
use std::thread::sleep;
use std::time::Duration;

#[test]
fn test_bubble_sort_implementation() {
    // Duration: ~500ms
    sleep(Duration::from_millis(500));

    fn bubble_sort<T: Ord + Clone>(arr: &mut [T]) {
        let n = arr.len();
        for i in 0..n {
            let mut swapped = false;
            for j in 0..n - 1 - i {
                if arr[j] > arr[j + 1] {
                    arr.swap(j, j + 1);
                    swapped = true;
                }
            }
            if !swapped {
                break;
            }
        }
    }

    // Test with various inputs
    let mut arr1 = vec![64, 34, 25, 12, 22, 11, 90];
    bubble_sort(&mut arr1);
    assert_eq!(arr1, vec![11, 12, 22, 25, 34, 64, 90]);

    // Already sorted
    let mut arr2 = vec![1, 2, 3, 4, 5];
    bubble_sort(&mut arr2);
    assert_eq!(arr2, vec![1, 2, 3, 4, 5]);

    // Reverse sorted
    let mut arr3: Vec<i32> = (0..100).rev().collect();
    bubble_sort(&mut arr3);
    assert_eq!(arr3, (0..100).collect::<Vec<_>>());

    // Empty and single element
    let mut arr4: Vec<i32> = vec![];
    bubble_sort(&mut arr4);
    assert!(arr4.is_empty());

    let mut arr5 = vec![42];
    bubble_sort(&mut arr5);
    assert_eq!(arr5, vec![42]);
}

#[test]
fn test_quicksort_implementation() {
    // Duration: ~1s
    sleep(Duration::from_secs(1));

    fn quicksort<T: Ord + Clone>(arr: &mut [T]) {
        if arr.len() <= 1 {
            return;
        }

        let pivot_idx = partition(arr);
        let (left, right) = arr.split_at_mut(pivot_idx);
        quicksort(left);
        quicksort(&mut right[1..]);
    }

    fn partition<T: Ord + Clone>(arr: &mut [T]) -> usize {
        let len = arr.len();
        let pivot_idx = len / 2;
        arr.swap(pivot_idx, len - 1);

        let mut i = 0;
        for j in 0..len - 1 {
            if arr[j] <= arr[len - 1] {
                arr.swap(i, j);
                i += 1;
            }
        }
        arr.swap(i, len - 1);
        i
    }

    // Test with random-ish data
    let mut arr: Vec<i32> = (0..1000).map(|i| (i * 17 + 31) % 500).collect();
    let mut expected = arr.clone();
    expected.sort();

    quicksort(&mut arr);
    assert_eq!(arr, expected);

    // Test with duplicates
    let mut arr2 = vec![3, 1, 4, 1, 5, 9, 2, 6, 5, 3, 5];
    quicksort(&mut arr2);
    assert_eq!(arr2, vec![1, 1, 2, 3, 3, 4, 5, 5, 5, 6, 9]);

    // Large sorted input
    let mut arr3: Vec<i32> = (0..500).collect();
    quicksort(&mut arr3);
    assert_eq!(arr3, (0..500).collect::<Vec<_>>());
}

#[test]
fn test_merge_sort_implementation() {
    // Duration: ~1.5s
    sleep(Duration::from_millis(1500));

    fn merge_sort<T: Ord + Clone>(arr: &mut [T]) {
        let len = arr.len();
        if len <= 1 {
            return;
        }

        let mid = len / 2;
        merge_sort(&mut arr[..mid]);
        merge_sort(&mut arr[mid..]);

        let left: Vec<T> = arr[..mid].to_vec();
        let right: Vec<T> = arr[mid..].to_vec();

        let mut i = 0;
        let mut j = 0;
        let mut k = 0;

        while i < left.len() && j < right.len() {
            if left[i] <= right[j] {
                arr[k] = left[i].clone();
                i += 1;
            } else {
                arr[k] = right[j].clone();
                j += 1;
            }
            k += 1;
        }

        while i < left.len() {
            arr[k] = left[i].clone();
            i += 1;
            k += 1;
        }

        while j < right.len() {
            arr[k] = right[j].clone();
            j += 1;
            k += 1;
        }
    }

    // Test stability with tuples
    let mut arr: Vec<(i32, char)> = vec![
        (3, 'a'), (1, 'b'), (2, 'c'), (1, 'd'), (3, 'e'), (2, 'f'),
    ];
    merge_sort(&mut arr);

    // Verify sorted
    assert!(arr.windows(2).all(|w| w[0].0 <= w[1].0));

    // Large test
    let mut arr2: Vec<i32> = (0..2000).map(|i| (i * 31) % 1000).collect();
    let mut expected = arr2.clone();
    expected.sort();

    merge_sort(&mut arr2);
    assert_eq!(arr2, expected);
}

#[test]
fn test_binary_search_variants() {
    // Duration: ~800ms
    sleep(Duration::from_millis(800));

    fn binary_search<T: Ord>(arr: &[T], target: &T) -> Option<usize> {
        let mut left = 0;
        let mut right = arr.len();

        while left < right {
            let mid = left + (right - left) / 2;
            match arr[mid].cmp(target) {
                std::cmp::Ordering::Equal => return Some(mid),
                std::cmp::Ordering::Less => left = mid + 1,
                std::cmp::Ordering::Greater => right = mid,
            }
        }
        None
    }

    fn lower_bound<T: Ord>(arr: &[T], target: &T) -> usize {
        let mut left = 0;
        let mut right = arr.len();

        while left < right {
            let mid = left + (right - left) / 2;
            if arr[mid] < *target {
                left = mid + 1;
            } else {
                right = mid;
            }
        }
        left
    }

    fn upper_bound<T: Ord>(arr: &[T], target: &T) -> usize {
        let mut left = 0;
        let mut right = arr.len();

        while left < right {
            let mid = left + (right - left) / 2;
            if arr[mid] <= *target {
                left = mid + 1;
            } else {
                right = mid;
            }
        }
        left
    }

    let arr: Vec<i32> = (0..10000).collect();

    // Basic binary search
    assert_eq!(binary_search(&arr, &0), Some(0));
    assert_eq!(binary_search(&arr, &9999), Some(9999));
    assert_eq!(binary_search(&arr, &5000), Some(5000));
    assert_eq!(binary_search(&arr, &10000), None);
    assert_eq!(binary_search(&arr, &-1), None);

    // Test with duplicates for bounds
    let dup_arr = vec![1, 2, 2, 2, 3, 3, 4, 5, 5, 5, 5];
    assert_eq!(lower_bound(&dup_arr, &2), 1);
    assert_eq!(upper_bound(&dup_arr, &2), 4);
    assert_eq!(lower_bound(&dup_arr, &5), 7);
    assert_eq!(upper_bound(&dup_arr, &5), 11);
    assert_eq!(lower_bound(&dup_arr, &6), 11);
}

#[test]
fn test_linear_search_and_find() {
    // Duration: ~600ms
    sleep(Duration::from_millis(600));

    fn linear_search<T: PartialEq>(arr: &[T], target: &T) -> Option<usize> {
        arr.iter().position(|x| x == target)
    }

    fn find_all<T: PartialEq>(arr: &[T], target: &T) -> Vec<usize> {
        arr.iter()
            .enumerate()
            .filter(|(_, x)| *x == target)
            .map(|(i, _)| i)
            .collect()
    }

    // Linear search tests
    let arr = vec![4, 2, 7, 1, 9, 3, 8, 5, 6, 0];
    assert_eq!(linear_search(&arr, &7), Some(2));
    assert_eq!(linear_search(&arr, &0), Some(9));
    assert_eq!(linear_search(&arr, &10), None);

    // Find all occurrences
    let arr2 = vec![1, 2, 3, 2, 4, 2, 5, 2, 6];
    let indices = find_all(&arr2, &2);
    assert_eq!(indices, vec![1, 3, 5, 7]);

    // Performance test with large array
    let large: Vec<i32> = (0..10000).collect();
    let mut found_count = 0;
    for target in 0..1000 {
        if linear_search(&large, &target).is_some() {
            found_count += 1;
        }
    }
    assert_eq!(found_count, 1000);
}

#[test]
fn test_sieve_of_eratosthenes() {
    // Duration: ~2s
    sleep(Duration::from_secs(2));

    fn sieve_of_eratosthenes(n: usize) -> Vec<bool> {
        let mut is_prime = vec![true; n + 1];
        is_prime[0] = false;
        if n >= 1 {
            is_prime[1] = false;
        }

        let mut p = 2;
        while p * p <= n {
            if is_prime[p] {
                let mut multiple = p * p;
                while multiple <= n {
                    is_prime[multiple] = false;
                    multiple += p;
                }
            }
            p += 1;
        }

        is_prime
    }

    let n = 10000;
    let sieve = sieve_of_eratosthenes(n);

    // Verify against is_prime function
    for i in 0..=n {
        let sieve_result = sieve[i];
        let func_result = is_prime(i as u64);
        assert_eq!(sieve_result, func_result, "Mismatch at {}", i);
    }

    // Count primes
    let prime_count = sieve.iter().filter(|&&p| p).count();
    assert_eq!(prime_count, 1229); // pi(10000) = 1229

    // Verify specific primes
    assert!(sieve[2]);
    assert!(sieve[3]);
    assert!(!sieve[4]);
    assert!(sieve[97]);
    assert!(!sieve[100]);
    assert!(sieve[9973]); // Largest prime < 10000
}

#[test]
fn test_euclidean_algorithm_extended() {
    // Duration: ~2.5s
    sleep(Duration::from_millis(2500));

    fn extended_gcd(a: i64, b: i64) -> (i64, i64, i64) {
        if b == 0 {
            return (a, 1, 0);
        }
        let (g, x, y) = extended_gcd(b, a % b);
        (g, y, x - (a / b) * y)
    }

    // Verify extended GCD
    for a in 1..100i64 {
        for b in 1..100i64 {
            let (g, x, y) = extended_gcd(a, b);

            // Verify GCD is correct
            assert_eq!(g as u64, gcd(a as u64, b as u64));

            // Verify Bezout's identity: ax + by = gcd(a, b)
            assert_eq!(a * x + b * y, g, "Bezout failed for ({}, {})", a, b);
        }
    }

    // Modular multiplicative inverse
    fn mod_inverse(a: i64, m: i64) -> Option<i64> {
        let (g, x, _) = extended_gcd(a, m);
        if g != 1 {
            None // Inverse doesn't exist
        } else {
            Some(((x % m) + m) % m)
        }
    }

    // Test modular inverse
    let m = 1000000007i64; // Large prime
    for a in [3, 7, 13, 17, 23, 31, 37, 41, 43, 47] {
        let inv = mod_inverse(a, m).unwrap();
        assert_eq!((a * inv) % m, 1, "Inverse failed for {}", a);
    }
}

#[test]
fn test_kadane_max_subarray() {
    // Duration: ~3s
    sleep(Duration::from_secs(3));

    fn kadane(arr: &[i32]) -> i32 {
        if arr.is_empty() {
            return 0;
        }

        let mut max_ending = arr[0];
        let mut max_so_far = arr[0];

        for &x in &arr[1..] {
            max_ending = std::cmp::max(x, max_ending + x);
            max_so_far = std::cmp::max(max_so_far, max_ending);
        }

        max_so_far
    }

    fn kadane_with_indices(arr: &[i32]) -> (i32, usize, usize) {
        if arr.is_empty() {
            return (0, 0, 0);
        }

        let mut max_ending = arr[0];
        let mut max_so_far = arr[0];
        let mut start = 0;
        let mut end = 0;
        let mut temp_start = 0;

        for (i, &x) in arr.iter().enumerate().skip(1) {
            if x > max_ending + x {
                max_ending = x;
                temp_start = i;
            } else {
                max_ending += x;
            }

            if max_ending > max_so_far {
                max_so_far = max_ending;
                start = temp_start;
                end = i;
            }
        }

        (max_so_far, start, end)
    }

    // Basic tests
    let arr1 = vec![-2, 1, -3, 4, -1, 2, 1, -5, 4];
    assert_eq!(kadane(&arr1), 6); // [4, -1, 2, 1]

    let arr2 = vec![1, 2, 3, 4, 5];
    assert_eq!(kadane(&arr2), 15);

    let arr3 = vec![-1, -2, -3, -4, -5];
    assert_eq!(kadane(&arr3), -1);

    // Test with indices
    let (sum, start, end) = kadane_with_indices(&arr1);
    assert_eq!(sum, 6);
    assert_eq!(start, 3);
    assert_eq!(end, 6);

    // Verify the subarray
    let subarray_sum: i32 = arr1[start..=end].iter().sum();
    assert_eq!(subarray_sum, sum);

    // Large array test
    let large: Vec<i32> = (0..5000).map(|i| if i % 2 == 0 { 1 } else { -1 }).collect();
    let max_sum = kadane(&large);
    assert_eq!(max_sum, 1);
}
