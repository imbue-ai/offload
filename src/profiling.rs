//! Profiling utilities for tracking time since program start.
//!
//! This module provides a global start time and logging helpers to trace
//! where time is spent during test execution.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Global start time, initialized when the program starts.
static START_TIME: OnceLock<Instant> = OnceLock::new();

/// Global timing log file.
static TIMING_LOG: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

/// Initialize the global start time. Call this at the very beginning of main().
pub fn init() {
    START_TIME.get_or_init(Instant::now);

    // Initialize timing log file
    TIMING_LOG.get_or_init(|| {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open("offload-timing.log")
            .expect("Failed to open offload-timing.log");
        Mutex::new(file)
    });
}

/// Get elapsed time since program start in seconds.
pub fn elapsed_secs() -> f64 {
    START_TIME
        .get()
        .map(|start| start.elapsed().as_secs_f64())
        .unwrap_or(0.0)
}

/// Write a message to the timing log file.
pub fn write_timing_log(msg: &str) {
    if let Some(log) = TIMING_LOG.get()
        && let Ok(mut file) = log.lock()
    {
        let _ = writeln!(file, "{}", msg);
        let _ = file.flush();
    }
}

/// Log a profiling event with elapsed time to both stderr and timing log file.
#[macro_export]
macro_rules! profile_log {
    ($($arg:tt)*) => {{
        let msg = format!("[{:>8.3}s] {}", $crate::profiling::elapsed_secs(), format!($($arg)*));
        eprintln!("{}", msg);
        $crate::profiling::write_timing_log(&msg);
    }};
}

/// Set the start time as an environment variable for child processes (e.g., Python scripts).
///
/// # Safety
/// This must be called early in main() before spawning any threads.
pub fn set_env_start_time() {
    if let Some(start) = START_TIME.get() {
        // Store as Unix timestamp in nanoseconds for precision
        let nanos_since_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            - start.elapsed().as_nanos();
        // SAFETY: Called at program start before any threads are spawned
        unsafe {
            std::env::set_var("OFFLOAD_START_TIME_NANOS", nanos_since_epoch.to_string());
        }
    }
}
