//! Collection operation tests - 10 tests for Vec, HashMap, and HashSet operations.

use rust_tests::{max_in_slice, sum_slice};
use std::collections::{HashMap, HashSet};
use std::thread::sleep;
use std::time::Duration;

#[test]
fn test_vec_creation_and_access() {
    // Duration: ~100ms
    sleep(Duration::from_millis(100));

    // Create vectors in various ways
    let v1: Vec<i32> = (0..1000).collect();
    let v2: Vec<i32> = vec![0; 1000];
    let v3: Vec<i32> = Vec::from_iter(0..1000);

    assert_eq!(v1.len(), 1000);
    assert_eq!(v2.len(), 1000);
    assert_eq!(v3.len(), 1000);

    // Access elements
    assert_eq!(v1[0], 0);
    assert_eq!(v1[999], 999);
    assert_eq!(v2[500], 0);

    // Using get for safe access
    assert_eq!(v1.get(0), Some(&0));
    assert_eq!(v1.get(999), Some(&999));
    assert_eq!(v1.get(1000), None);

    // First and last
    assert_eq!(v1.first(), Some(&0));
    assert_eq!(v1.last(), Some(&999));
}

#[test]
fn test_vec_modification() {
    // Duration: ~300ms
    sleep(Duration::from_millis(300));

    let mut vec: Vec<i32> = Vec::new();

    // Push elements
    for i in 0..500 {
        vec.push(i);
    }
    assert_eq!(vec.len(), 500);

    // Pop elements
    for _ in 0..100 {
        vec.pop();
    }
    assert_eq!(vec.len(), 400);

    // Insert in middle
    vec.insert(200, -1);
    assert_eq!(vec[200], -1);
    assert_eq!(vec.len(), 401);

    // Remove from middle
    vec.remove(200);
    assert_eq!(vec.len(), 400);
    assert_eq!(vec[200], 200);

    // Extend
    vec.extend(500..600);
    assert_eq!(vec.len(), 500);

    // Truncate
    vec.truncate(250);
    assert_eq!(vec.len(), 250);

    // Clear
    vec.clear();
    assert!(vec.is_empty());
}

#[test]
fn test_vec_slice_operations() {
    // Duration: ~500ms
    sleep(Duration::from_millis(500));

    // Create large vector
    let vec: Vec<i32> = (1..=10000).collect();

    // Test sum_slice from library
    let sum = sum_slice(&vec);
    assert_eq!(sum, 50005000);

    // Test max_in_slice from library
    let max = max_in_slice(&vec);
    assert_eq!(max, Some(10000));

    // Slice operations
    let slice = &vec[0..100];
    assert_eq!(slice.len(), 100);
    assert_eq!(sum_slice(slice), 5050);

    let middle_slice = &vec[4000..5000];
    assert_eq!(middle_slice.len(), 1000);
    assert_eq!(middle_slice[0], 4001);

    // Chunks
    let chunks: Vec<_> = vec.chunks(1000).collect();
    assert_eq!(chunks.len(), 10);
    assert_eq!(chunks[0].len(), 1000);
    assert_eq!(sum_slice(chunks[0]), 500500);
}

#[test]
fn test_vec_sorting_and_searching() {
    // Duration: ~800ms
    sleep(Duration::from_millis(800));

    // Create unsorted vector
    let mut vec: Vec<i32> = (0..5000).rev().collect();
    assert_eq!(vec[0], 4999);

    // Sort
    vec.sort();
    assert_eq!(vec[0], 0);
    assert_eq!(vec[4999], 4999);

    // Binary search
    assert_eq!(vec.binary_search(&0), Ok(0));
    assert_eq!(vec.binary_search(&4999), Ok(4999));
    assert_eq!(vec.binary_search(&2500), Ok(2500));
    assert!(vec.binary_search(&5000).is_err());

    // Sort by key
    let mut pairs: Vec<(i32, i32)> = (0..1000).map(|i| (i % 10, i)).collect();
    pairs.sort_by_key(|&(k, _)| k);
    assert!(pairs.windows(2).all(|w| w[0].0 <= w[1].0));

    // Reverse sort
    vec.sort_by(|a, b| b.cmp(a));
    assert_eq!(vec[0], 4999);
    assert_eq!(vec[4999], 0);
}

#[test]
fn test_vec_iterators() {
    // Duration: ~600ms
    sleep(Duration::from_millis(600));

    let vec: Vec<i32> = (1..=1000).collect();

    // Map
    let doubled: Vec<i32> = vec.iter().map(|x| x * 2).collect();
    assert_eq!(doubled[0], 2);
    assert_eq!(doubled[999], 2000);

    // Filter
    let evens: Vec<i32> = vec.iter().filter(|&x| x % 2 == 0).copied().collect();
    assert_eq!(evens.len(), 500);
    assert_eq!(evens[0], 2);

    // Filter map
    let squares_of_evens: Vec<i32> = vec
        .iter()
        .filter(|&x| x % 2 == 0)
        .map(|x| x * x)
        .collect();
    assert_eq!(squares_of_evens.len(), 500);
    assert_eq!(squares_of_evens[0], 4);

    // Fold/reduce
    let product: i64 = vec.iter().take(10).map(|&x| x as i64).product();
    assert_eq!(product, 3628800); // 10!

    // Any/all
    assert!(vec.iter().any(|&x| x == 500));
    assert!(vec.iter().all(|&x| x > 0));
    assert!(!vec.iter().any(|&x| x > 1000));
}

#[test]
fn test_hashmap_basic_operations() {
    // Duration: ~700ms
    sleep(Duration::from_millis(700));

    let mut map: HashMap<String, i32> = HashMap::new();

    // Insert
    for i in 0..1000 {
        map.insert(format!("key_{}", i), i);
    }
    assert_eq!(map.len(), 1000);

    // Get
    assert_eq!(map.get("key_0"), Some(&0));
    assert_eq!(map.get("key_999"), Some(&999));
    assert_eq!(map.get("key_1000"), None);

    // Contains
    assert!(map.contains_key("key_500"));
    assert!(!map.contains_key("nonexistent"));

    // Entry API
    map.entry("key_0".to_string()).and_modify(|v| *v += 100);
    assert_eq!(map.get("key_0"), Some(&100));

    map.entry("new_key".to_string()).or_insert(42);
    assert_eq!(map.get("new_key"), Some(&42));

    // Remove
    let removed = map.remove("key_500");
    assert_eq!(removed, Some(500));
    assert!(!map.contains_key("key_500"));
}

#[test]
fn test_hashmap_iteration_and_aggregation() {
    // Duration: ~900ms
    sleep(Duration::from_millis(900));

    let mut map: HashMap<i32, i32> = HashMap::new();
    for i in 0..500 {
        map.insert(i, i * i);
    }

    // Iterate over keys
    let key_sum: i32 = map.keys().sum();
    assert_eq!(key_sum, 124750); // Sum of 0..500

    // Iterate over values
    let value_sum: i64 = map.values().map(|&v| v as i64).sum();
    assert_eq!(value_sum, 41541750); // Sum of squares 0..500

    // Iterate over entries
    let entry_count = map.iter().count();
    assert_eq!(entry_count, 500);

    // Filter and collect
    let even_keys: HashMap<i32, i32> = map
        .iter()
        .filter(|(&k, _)| k % 2 == 0)
        .map(|(&k, &v)| (k, v))
        .collect();
    assert_eq!(even_keys.len(), 250);

    // Mutable iteration
    for (_, v) in map.iter_mut() {
        *v += 1;
    }
    assert_eq!(map.get(&0), Some(&1)); // 0*0 + 1 = 1
    assert_eq!(map.get(&10), Some(&101)); // 10*10 + 1 = 101
}

#[test]
fn test_hashset_operations() {
    // Duration: ~1s
    sleep(Duration::from_secs(1));

    let mut set1: HashSet<i32> = (0..500).collect();
    let mut set2: HashSet<i32> = (250..750).collect();

    assert_eq!(set1.len(), 500);
    assert_eq!(set2.len(), 500);

    // Contains
    assert!(set1.contains(&0));
    assert!(set1.contains(&499));
    assert!(!set1.contains(&500));

    // Union
    let union: HashSet<i32> = set1.union(&set2).copied().collect();
    assert_eq!(union.len(), 750); // 0..750

    // Intersection
    let intersection: HashSet<i32> = set1.intersection(&set2).copied().collect();
    assert_eq!(intersection.len(), 250); // 250..500

    // Difference
    let diff: HashSet<i32> = set1.difference(&set2).copied().collect();
    assert_eq!(diff.len(), 250); // 0..250

    // Symmetric difference
    let sym_diff: HashSet<i32> = set1.symmetric_difference(&set2).copied().collect();
    assert_eq!(sym_diff.len(), 500); // 0..250 and 500..750

    // Insert and remove
    set1.insert(1000);
    assert!(set1.contains(&1000));
    set1.remove(&1000);
    assert!(!set1.contains(&1000));
}

#[test]
fn test_hashset_subset_superset() {
    // Duration: ~1.2s
    sleep(Duration::from_millis(1200));

    let full_set: HashSet<i32> = (0..1000).collect();
    let subset: HashSet<i32> = (100..200).collect();
    let disjoint: HashSet<i32> = (1000..1100).collect();

    // Subset/superset checks
    assert!(subset.is_subset(&full_set));
    assert!(full_set.is_superset(&subset));
    assert!(!full_set.is_subset(&subset));
    assert!(!subset.is_superset(&full_set));

    // Disjoint check
    assert!(full_set.is_disjoint(&disjoint));
    assert!(!full_set.is_disjoint(&subset));

    // Build subsets and verify relationships
    let mut sets: Vec<HashSet<i32>> = Vec::new();
    for i in 0..10 {
        let s: HashSet<i32> = (i * 100..(i + 1) * 100).collect();
        sets.push(s);
    }

    // Each pair of different sets should be disjoint
    for i in 0..10 {
        for j in 0..10 {
            if i != j {
                assert!(sets[i].is_disjoint(&sets[j]));
            }
        }
    }

    // Each set should be subset of full_set
    for s in &sets {
        assert!(s.is_subset(&full_set));
    }
}

#[test]
fn test_collection_conversions() {
    // Duration: ~1.5s
    sleep(Duration::from_millis(1500));

    // Vec to HashSet
    let vec: Vec<i32> = (0..1000).chain(0..500).collect(); // Contains duplicates
    let set: HashSet<i32> = vec.iter().copied().collect();
    assert_eq!(vec.len(), 1500);
    assert_eq!(set.len(), 1000);

    // HashSet to Vec
    let set: HashSet<i32> = (0..500).collect();
    let mut vec: Vec<i32> = set.iter().copied().collect();
    vec.sort();
    assert_eq!(vec.len(), 500);
    assert_eq!(vec[0], 0);
    assert_eq!(vec[499], 499);

    // Vec to HashMap
    let pairs: Vec<(String, i32)> = (0..500).map(|i| (format!("key_{}", i), i)).collect();
    let map: HashMap<String, i32> = pairs.into_iter().collect();
    assert_eq!(map.len(), 500);
    assert_eq!(map.get("key_100"), Some(&100));

    // HashMap to Vec of tuples
    let map: HashMap<i32, i32> = (0..500).map(|i| (i, i * 2)).collect();
    let mut vec: Vec<(i32, i32)> = map.into_iter().collect();
    vec.sort_by_key(|&(k, _)| k);
    assert_eq!(vec.len(), 500);
    assert_eq!(vec[100], (100, 200));

    // Chain multiple conversions
    let original: Vec<i32> = (0..1000).collect();
    let set: HashSet<i32> = original.iter().copied().collect();
    let map: HashMap<i32, i32> = set.iter().map(|&x| (x, x * x)).collect();
    let final_vec: Vec<i32> = map.values().copied().collect();
    assert_eq!(final_vec.len(), 1000);
}
