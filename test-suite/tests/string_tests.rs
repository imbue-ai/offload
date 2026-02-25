//! String operation tests - 10 tests for string parsing and manipulation.

use rust_tests::{is_palindrome, reverse_string};
use std::thread::sleep;
use std::time::Duration;

#[test]
fn test_string_reversal_basic() {
    // Duration: ~100ms
    sleep(Duration::from_millis(100));

    // Basic reversals
    assert_eq!(reverse_string("hello"), "olleh");
    assert_eq!(reverse_string("world"), "dlrow");
    assert_eq!(reverse_string("a"), "a");
    assert_eq!(reverse_string(""), "");
    assert_eq!(reverse_string("ab"), "ba");
    assert_eq!(reverse_string("abc"), "cba");

    // Sentences
    assert_eq!(reverse_string("The quick brown fox"), "xof nworb kciuq ehT");

    // Numbers as strings
    assert_eq!(reverse_string("12345"), "54321");
    assert_eq!(reverse_string("1234567890"), "0987654321");
}

#[test]
fn test_string_reversal_unicode() {
    // Duration: ~300ms
    sleep(Duration::from_millis(300));

    // Unicode characters
    assert_eq!(reverse_string("hello world"), "dlrow olleh");
    assert_eq!(reverse_string("unicode: cafe"), "efac :edocinu");

    // Chinese characters
    assert_eq!(reverse_string("hello"), "olleh");

    // Emoji (grapheme clusters may behave differently)
    let emoji_str = "abc";
    let reversed = reverse_string(emoji_str);
    assert_eq!(reversed, "cba");

    // Mixed ASCII and extended characters
    assert_eq!(reverse_string("rust-lang"), "gnal-tsur");
}

#[test]
fn test_string_reversal_bulk() {
    // Duration: ~500ms
    sleep(Duration::from_millis(500));

    // Generate and reverse many strings
    let mut reversed_strings = Vec::new();
    for i in 0..2000 {
        let s = format!("string_number_{:05}", i);
        let rev = reverse_string(&s);
        reversed_strings.push(rev);
    }

    // Verify some specific reversals
    assert_eq!(reversed_strings[0], "00000_rebmun_gnirts");
    assert_eq!(reversed_strings[100], "00100_rebmun_gnirts");
    assert_eq!(reversed_strings[999], "99900_rebmun_gnirts");
    assert_eq!(reversed_strings[1999], "99910_rebmun_gnirts");

    // Double reversal should give original
    for i in [0, 100, 500, 1000, 1500, 1999] {
        let original = format!("string_number_{:05}", i);
        let double_reversed = reverse_string(&reversed_strings[i]);
        assert_eq!(double_reversed, original);
    }
}

#[test]
fn test_palindrome_basic() {
    // Duration: ~200ms
    sleep(Duration::from_millis(200));

    // Single words
    assert!(is_palindrome("racecar"));
    assert!(is_palindrome("level"));
    assert!(is_palindrome("radar"));
    assert!(is_palindrome("civic"));
    assert!(is_palindrome("noon"));
    assert!(is_palindrome("refer"));
    assert!(is_palindrome("deified"));
    assert!(is_palindrome("rotator"));

    // Non-palindromes
    assert!(!is_palindrome("hello"));
    assert!(!is_palindrome("world"));
    assert!(!is_palindrome("rust"));
    assert!(!is_palindrome("programming"));

    // Edge cases
    assert!(is_palindrome(""));
    assert!(is_palindrome("a"));
    assert!(is_palindrome("aa"));
    assert!(is_palindrome("aba"));
    assert!(!is_palindrome("ab"));
}

#[test]
fn test_palindrome_sentences() {
    // Duration: ~400ms
    sleep(Duration::from_millis(400));

    // Famous palindrome sentences
    assert!(is_palindrome("A man a plan a canal Panama"));
    assert!(is_palindrome("Was it a car or a cat I saw"));
    assert!(is_palindrome("No lemon no melon"));
    assert!(is_palindrome("Never odd or even"));
    assert!(is_palindrome("Do geese see God"));
    assert!(is_palindrome("A Santa at NASA"));
    assert!(is_palindrome("Madam Im Adam"));
    assert!(is_palindrome("Step on no pets"));

    // Non-palindrome sentences
    assert!(!is_palindrome("Hello World"));
    assert!(!is_palindrome("The quick brown fox"));
    assert!(!is_palindrome("Rust is awesome"));
}

#[test]
fn test_palindrome_numbers_as_strings() {
    // Duration: ~600ms
    sleep(Duration::from_millis(600));

    // Palindrome numbers
    assert!(is_palindrome("121"));
    assert!(is_palindrome("1221"));
    assert!(is_palindrome("12321"));
    assert!(is_palindrome("1234321"));
    assert!(is_palindrome("123454321"));
    assert!(is_palindrome("1"));
    assert!(is_palindrome("11"));
    assert!(is_palindrome("111"));

    // Non-palindrome numbers
    assert!(!is_palindrome("123"));
    assert!(!is_palindrome("1234"));
    assert!(!is_palindrome("12"));
    assert!(!is_palindrome("100"));

    // Generate and test palindrome numbers
    let mut palindrome_count = 0;
    for i in 1..10000 {
        let s = i.to_string();
        if is_palindrome(&s) {
            palindrome_count += 1;
        }
    }
    // Count of palindrome numbers from 1 to 9999
    // 1-9: 9, 11-99: 9, 101-999: 90, 1001-9999: 90 = 198
    assert_eq!(palindrome_count, 198);
}

#[test]
fn test_string_parsing_integers() {
    // Duration: ~800ms
    sleep(Duration::from_millis(800));

    // Parse various integer formats
    let test_cases = vec![
        ("123", 123i64),
        ("-456", -456),
        ("0", 0),
        ("+789", 789),
        ("1000000", 1000000),
        ("-999999", -999999),
    ];

    for (input, expected) in test_cases {
        let parsed: i64 = input.parse().expect("Failed to parse integer");
        assert_eq!(parsed, expected);
    }

    // Parse and compute sum
    let numbers: Vec<&str> = vec!["10", "20", "30", "40", "50", "60", "70", "80", "90", "100"];
    let sum: i64 = numbers.iter().map(|s| s.parse::<i64>().unwrap()).sum();
    assert_eq!(sum, 550);

    // Parse with validation
    let valid_count: usize = (1..1000)
        .map(|i| format!("{}", i))
        .filter(|s| s.parse::<i32>().is_ok())
        .count();
    assert_eq!(valid_count, 999);
}

#[test]
fn test_string_parsing_floats() {
    // Duration: ~1s
    sleep(Duration::from_secs(1));

    // Parse floating point numbers
    let test_cases = vec![
        ("3.14159", 3.14159f64),
        ("-2.71828", -2.71828),
        ("0.0", 0.0),
        ("1e10", 1e10),
        ("2.5e-3", 2.5e-3),
        ("100.0", 100.0),
    ];

    for (input, expected) in test_cases {
        let parsed: f64 = input.parse().expect("Failed to parse float");
        assert!((parsed - expected).abs() < 1e-10, "Mismatch for {}", input);
    }

    // Parse and compute average
    let float_strings: Vec<String> = (1..=100).map(|i| format!("{}.5", i)).collect();
    let sum: f64 = float_strings.iter().map(|s| s.parse::<f64>().unwrap()).sum();
    let average = sum / 100.0;
    assert!((average - 51.0).abs() < 0.001);
}

#[test]
fn test_string_manipulation_split_join() {
    // Duration: ~1.5s
    sleep(Duration::from_millis(1500));

    // Split and join operations
    let sentence = "the quick brown fox jumps over the lazy dog";
    let words: Vec<&str> = sentence.split_whitespace().collect();
    assert_eq!(words.len(), 9);
    assert_eq!(words[0], "the");
    assert_eq!(words[4], "jumps");
    assert_eq!(words[8], "dog");

    // Join words back
    let rejoined = words.join(" ");
    assert_eq!(rejoined, sentence);

    // Split by various delimiters
    let csv_line = "apple,banana,cherry,date,elderberry";
    let fruits: Vec<&str> = csv_line.split(',').collect();
    assert_eq!(fruits.len(), 5);
    assert_eq!(fruits[2], "cherry");

    // Process many lines
    let mut total_words = 0;
    for i in 0..1000 {
        let line = format!("word1 word2 word3 item{}", i);
        let word_count = line.split_whitespace().count();
        total_words += word_count;
    }
    assert_eq!(total_words, 4000);
}

#[test]
fn test_string_case_transformations() {
    // Duration: ~2s
    sleep(Duration::from_secs(2));

    // Case transformations
    let test_strings = vec![
        "Hello World",
        "RUST PROGRAMMING",
        "mixed CASE string",
        "123 Numbers 456",
        "special !@# chars",
    ];

    for s in &test_strings {
        let upper = s.to_uppercase();
        let lower = s.to_lowercase();

        // Verify transformations
        assert_eq!(upper, upper.to_uppercase());
        assert_eq!(lower, lower.to_lowercase());

        // Case-insensitive comparison
        assert_eq!(upper.to_lowercase(), lower);
    }

    // Process many strings with case transformations
    let mut uppercase_chars = 0;
    let mut lowercase_chars = 0;
    for i in 0..2000 {
        let s = format!("TestString{:04}", i);
        for c in s.chars() {
            if c.is_uppercase() {
                uppercase_chars += 1;
            }
            if c.is_lowercase() {
                lowercase_chars += 1;
            }
        }
    }

    // Each string "TestString0000" has 2 uppercase (T, S) and 8 lowercase (estrtring)
    assert_eq!(uppercase_chars, 4000);
    assert_eq!(lowercase_chars, 16000);

    // Title case simulation (capitalize first letter of each word)
    let sentence = "the rust programming language";
    let title_case: String = sentence
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(title_case, "The Rust Programming Language");
}
