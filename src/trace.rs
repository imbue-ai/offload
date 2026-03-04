//! Chrome Trace Event Format output for performance visualization in Perfetto UI.

use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Process ID for the local (orchestrator) process.
pub const PID_LOCAL: u32 = 0;

/// Thread ID for the main thread.
pub const TID_MAIN: u32 = 0;

/// Thread ID for the API thread.
pub const TID_API: u32 = 0;

/// Thread ID for the execution thread.
pub const TID_EXEC: u32 = 1;

/// Thread ID for the I/O thread.
pub const TID_IO: u32 = 2;

/// Returns the process ID for a sandbox at the given index.
pub fn sandbox_pid(index: usize) -> u32 {
    (index as u32) + 1
}

/// A single Chrome Trace Event.
#[derive(Debug, Clone, Serialize)]
pub struct TraceEvent {
    /// Event name.
    pub name: String,
    /// Event category.
    pub cat: String,
    /// Phase type (e.g. "X" for complete, "i" for instant, "M" for metadata).
    pub ph: String,
    /// Timestamp in microseconds since trace epoch.
    pub ts: f64,
    /// Duration in microseconds (only for complete events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dur: Option<f64>,
    /// Process ID.
    pub pid: u32,
    /// Thread ID.
    pub tid: u32,
    /// Optional event arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
}

/// Holds trace state for an active tracing session.
pub struct ActiveTracer {
    /// The epoch from which all timestamps are measured.
    epoch: Instant,
    /// Collected trace events.
    events: Mutex<Vec<TraceEvent>>,
}

impl ActiveTracer {
    /// Returns microseconds elapsed since the tracer epoch.
    pub fn elapsed_us(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64() * 1_000_000.0
    }

    /// Pushes a trace event into the event buffer.
    pub fn record(&self, event: TraceEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }
}

/// Performance tracer that emits Chrome Trace Event Format data.
#[derive(Clone)]
pub enum Tracer {
    /// Actively collecting trace events.
    Active(Arc<ActiveTracer>),
    /// No-op tracer that discards all events.
    Noop,
}

impl Default for Tracer {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracer {
    /// Creates a new active tracer with the current instant as epoch.
    pub fn new() -> Self {
        Tracer::Active(Arc::new(ActiveTracer {
            epoch: Instant::now(),
            events: Mutex::new(Vec::new()),
        }))
    }

    /// Creates a no-op tracer that discards all events.
    pub fn noop() -> Self {
        Tracer::Noop
    }

    /// Records a complete ("X") event with explicit start time and duration.
    #[allow(clippy::too_many_arguments)]
    pub fn complete_event(
        &self,
        name: &str,
        cat: &str,
        pid: u32,
        tid: u32,
        start_us: f64,
        dur_us: f64,
        args: Option<serde_json::Value>,
    ) {
        if let Tracer::Active(inner) = self {
            inner.record(TraceEvent {
                name: name.to_string(),
                cat: cat.to_string(),
                ph: "X".to_string(),
                ts: start_us,
                dur: Some(dur_us),
                pid,
                tid,
                args,
            });
        }
    }

    /// Records an instant ("i") event with thread scope.
    pub fn instant_event(
        &self,
        name: &str,
        cat: &str,
        pid: u32,
        tid: u32,
        args: Option<serde_json::Value>,
    ) {
        if let Tracer::Active(inner) = self {
            let ts = inner.elapsed_us();
            inner.record(TraceEvent {
                name: name.to_string(),
                cat: cat.to_string(),
                ph: "i".to_string(),
                ts,
                dur: None,
                pid,
                tid,
                args: {
                    // Merge scope into args
                    let scope_obj = serde_json::json!({"s": "t"});
                    match args {
                        Some(serde_json::Value::Object(mut map)) => {
                            map.insert("s".to_string(), serde_json::Value::String("t".to_string()));
                            Some(serde_json::Value::Object(map))
                        }
                        Some(other) => {
                            let mut map = serde_json::Map::new();
                            map.insert("s".to_string(), serde_json::Value::String("t".to_string()));
                            map.insert("original_args".to_string(), other);
                            Some(serde_json::Value::Object(map))
                        }
                        None => Some(scope_obj),
                    }
                },
            });
        }
    }

    /// Records a metadata ("M") event (e.g. for process_name, thread_name).
    pub fn metadata_event(&self, name: &str, pid: u32, tid: u32, args: serde_json::Value) {
        if let Tracer::Active(inner) = self {
            inner.record(TraceEvent {
                name: name.to_string(),
                cat: String::new(),
                ph: "M".to_string(),
                ts: 0.0,
                dur: None,
                pid,
                tid,
                args: Some(args),
            });
        }
    }

    /// Creates an RAII span guard that records a complete event on drop.
    pub fn span(&self, name: &str, cat: &str, pid: u32, tid: u32) -> SpanGuard {
        let start_us = self.elapsed_us();
        SpanGuard {
            tracer: self.clone(),
            name: name.to_string(),
            cat: cat.to_string(),
            pid,
            tid,
            start_us,
            args: None,
        }
    }

    /// Returns microseconds elapsed since the tracer epoch (0.0 for Noop).
    pub fn elapsed_us(&self) -> f64 {
        match self {
            Tracer::Active(inner) => inner.elapsed_us(),
            Tracer::Noop => 0.0,
        }
    }

    /// Serializes all collected events to a JSON array string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        match self {
            Tracer::Active(inner) => {
                let events = inner
                    .events
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                serde_json::to_string(&*events)
            }
            Tracer::Noop => Ok("[]".to_string()),
        }
    }

    /// Writes the JSON trace to a file (no-op for Noop).
    pub fn write_to_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        match self {
            Tracer::Active(_) => {
                let json = self.to_json().map_err(std::io::Error::other)?;
                std::fs::write(path, json)
            }
            Tracer::Noop => Ok(()),
        }
    }
}

/// RAII guard that records a complete ("X") event when dropped.
pub struct SpanGuard {
    tracer: Tracer,
    name: String,
    cat: String,
    pid: u32,
    tid: u32,
    start_us: f64,
    /// Optional arguments to attach to the event.
    args: Option<serde_json::Value>,
}

impl SpanGuard {
    /// Attaches arguments to be included in the trace event on drop.
    pub fn with_args(mut self, args: serde_json::Value) -> Self {
        self.args = Some(args);
        self
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        let dur_us = self.tracer.elapsed_us() - self.start_us;
        self.tracer.complete_event(
            &self.name,
            &self.cat,
            self.pid,
            self.tid,
            self.start_us,
            dur_us,
            self.args.take(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_noop_tracer_is_noop() -> anyhow::Result<()> {
        let tracer = Tracer::noop();

        // Methods don't panic
        tracer.complete_event("test", "cat", 0, 0, 0.0, 1.0, None);
        tracer.instant_event("test", "cat", 0, 0, None);
        tracer.metadata_event("test", 0, 0, serde_json::json!({"name": "main"}));
        let _guard = tracer.span("test", "cat", 0, 0);
        drop(_guard);

        // to_json returns empty array
        let json = tracer.to_json()?;
        assert_eq!(json, "[]");

        // write_to_file is ok (doesn't actually write)
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("trace.json");
        assert!(tracer.write_to_file(&path).is_ok());
        assert!(!path.exists());

        Ok(())
    }

    #[test]
    fn test_active_tracer_collects_events() -> anyhow::Result<()> {
        let tracer = Tracer::new();

        tracer.complete_event("compile", "build", 0, 0, 0.0, 100.0, None);
        tracer.instant_event("checkpoint", "debug", 0, 0, None);

        let json = tracer.to_json()?;
        let events: Vec<serde_json::Value> = serde_json::from_str(&json)?;
        assert_eq!(events.len(), 2);

        Ok(())
    }

    #[test]
    fn test_span_guard_measures_duration() -> anyhow::Result<()> {
        let tracer = Tracer::new();

        {
            let _guard = tracer.span("sleep_span", "test", 0, 0);
            std::thread::sleep(Duration::from_millis(10));
        }

        let json = tracer.to_json()?;
        let events: Vec<serde_json::Value> = serde_json::from_str(&json)?;
        assert_eq!(events.len(), 1);

        let event = &events[0];
        assert_eq!(event["ph"], "X");
        assert_eq!(event["name"], "sleep_span");
        let dur = event["dur"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("dur field missing or not f64"))?;
        assert!(dur > 0.0, "duration should be > 0, got {dur}");

        Ok(())
    }

    #[test]
    fn test_json_output_valid() -> anyhow::Result<()> {
        let tracer = Tracer::new();

        tracer.metadata_event(
            "process_name",
            0,
            0,
            serde_json::json!({"name": "orchestrator"}),
        );
        tracer.complete_event("run_tests", "exec", 0, 1, 100.0, 500.0, None);
        tracer.instant_event(
            "marker",
            "debug",
            0,
            0,
            Some(serde_json::json!({"info": "start"})),
        );

        let json = tracer.to_json()?;
        let events: Vec<serde_json::Value> = serde_json::from_str(&json)?;
        assert_eq!(events.len(), 3);

        // Metadata event
        let meta = &events[0];
        assert_eq!(meta["ph"], "M");
        assert_eq!(meta["name"], "process_name");
        assert_eq!(meta["args"]["name"], "orchestrator");

        // Complete event
        let complete = &events[1];
        assert_eq!(complete["ph"], "X");
        assert_eq!(complete["name"], "run_tests");
        assert_eq!(complete["cat"], "exec");
        assert_eq!(complete["ts"], 100.0);
        assert_eq!(complete["dur"], 500.0);
        assert_eq!(complete["tid"], 1);

        // Instant event
        let instant = &events[2];
        assert_eq!(instant["ph"], "i");
        assert_eq!(instant["name"], "marker");
        assert_eq!(instant["args"]["info"], "start");
        assert_eq!(instant["args"]["s"], "t");

        Ok(())
    }

    #[test]
    fn test_sandbox_pid() {
        assert_eq!(sandbox_pid(0), 1);
        assert_eq!(sandbox_pid(5), 6);
    }
}
