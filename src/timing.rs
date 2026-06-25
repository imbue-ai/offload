//! Emits paired `starting:`/`finished:` timing logs via `tracing`.

use std::time::{Duration, Instant};

/// Starts a timing span, logging `starting: {name}` immediately and emitting
/// `finished: {name} [..., took {elapsed}]` when the guard is dropped or
/// [`TimedSpan::finish`] is called.
pub fn timed_span(name: impl Into<String>) -> TimedSpan {
    let name = name.into();
    tracing::info!("{}", starting_message(&name, None));
    TimedSpan {
        name,
        start: Instant::now(),
        annotations: Vec::new(),
        finished: false,
    }
}

/// Like [`timed_span`], but logs an initial `detail` in the start line and
/// retains it so it also appears in the finish line.
pub fn timed_span_with(name: impl Into<String>, detail: impl std::fmt::Display) -> TimedSpan {
    let name = name.into();
    let detail = detail.to_string();
    tracing::info!("{}", starting_message(&name, Some(&detail)));
    TimedSpan {
        name,
        start: Instant::now(),
        annotations: vec![detail],
        finished: false,
    }
}

/// Scoped guard that emits a `finished:` timing log on drop.
#[must_use = "a TimedSpan emits its finish log on drop; bind it to a variable for the duration of the operation"]
pub struct TimedSpan {
    name: String,
    start: Instant,
    annotations: Vec<String>,
    finished: bool,
}

impl TimedSpan {
    /// Appends an annotation that appears only in the finish line.
    pub fn annotate(&mut self, detail: impl std::fmt::Display) {
        self.annotations.push(detail.to_string());
    }

    /// Emits the finish line now, suppressing the drop-time emit. Use when the
    /// end of the operation does not coincide with a lexical scope.
    pub fn finish(mut self) {
        self.emit_finished();
    }

    fn emit_finished(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        tracing::info!(
            "{}",
            finished_message(&self.name, &self.annotations, self.start.elapsed())
        );
    }
}

impl Drop for TimedSpan {
    fn drop(&mut self) {
        self.emit_finished();
    }
}

fn starting_message(name: &str, detail: Option<&str>) -> String {
    match detail {
        Some(detail) => format!("starting: {name} [{detail}]"),
        None => format!("starting: {name}"),
    }
}

fn finished_message(name: &str, annotations: &[String], elapsed: Duration) -> String {
    let elapsed = format_elapsed(elapsed);
    let mut parts: Vec<String> = annotations.to_vec();
    parts.push(format!("took {elapsed}"));
    format!("finished: {name} [{}]", parts.join(", "))
}

fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 1.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}ms", d.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_elapsed_seconds() {
        assert_eq!(format_elapsed(Duration::from_secs_f64(10.25)), "10.2s");
        assert_eq!(format_elapsed(Duration::from_secs(1)), "1.0s");
    }

    #[test]
    fn format_elapsed_millis() {
        assert_eq!(format_elapsed(Duration::from_millis(250)), "250ms");
        assert_eq!(format_elapsed(Duration::from_millis(999)), "999ms");
    }

    #[test]
    fn starting_message_format() {
        assert_eq!(starting_message("upload", None), "starting: upload");
        assert_eq!(
            starting_message("upload", Some("10 files")),
            "starting: upload [10 files]"
        );
    }

    #[test]
    fn finished_message_format() {
        assert_eq!(
            finished_message("upload", &[], Duration::from_secs(1)),
            "finished: upload [took 1.0s]"
        );
        assert_eq!(
            finished_message(
                "upload",
                &["10 files".into(), "2300 KB".into()],
                Duration::from_secs(1)
            ),
            "finished: upload [10 files, 2300 KB, took 1.0s]"
        );
    }

    #[test]
    fn span_finish_does_not_panic() {
        let mut span = timed_span("op");
        span.annotate("done");
        span.finish();
    }

    #[test]
    fn span_drop_does_not_panic() {
        let mut span = timed_span("op");
        span.annotate("done");
        drop(span);
    }
}
