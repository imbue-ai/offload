//! JSONL serialization for test history records.
//!
//! The history file stores one JSON object per line, sorted by key (config, test_id).
//! Each record has a compact format with "k" for key tuple and "v" for values.

use super::HistoryError;
use serde::{Deserialize, Serialize};

/// Compact sample representation for JSONL: [run_id, timestamp_ms, duration_secs].
///
/// Serializes as a JSON array instead of an object for compactness.
#[derive(Debug, Clone)]
pub struct CompactSample(pub String, pub u64, pub f64);

impl Serialize for CompactSample {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeTuple;
        let mut tup = serializer.serialize_tuple(3)?;
        tup.serialize_element(&self.0)?;
        tup.serialize_element(&self.1)?;
        tup.serialize_element(&self.2)?;
        tup.end()
    }
}

impl<'de> Deserialize<'de> for CompactSample {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (run_id, timestamp_ms, duration_secs): (String, u64, f64) =
            Deserialize::deserialize(deserializer)?;
        Ok(CompactSample(run_id, timestamp_ms, duration_secs))
    }
}

impl From<&super::reservoir::Sample> for CompactSample {
    fn from(s: &super::reservoir::Sample) -> Self {
        CompactSample(s.run_id.clone(), s.timestamp_ms, s.duration_secs)
    }
}

impl From<CompactSample> for super::reservoir::Sample {
    fn from(c: CompactSample) -> Self {
        super::reservoir::Sample {
            run_id: c.0,
            timestamp_ms: c.1,
            duration_secs: c.2,
        }
    }
}

/// A single test's history record, stored in JSONL format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRecord {
    /// Key tuple: (config_filename, test_id).
    #[serde(rename = "k")]
    pub key: (String, String),

    /// Values for this test.
    #[serde(rename = "v")]
    pub values: TestValues,
}

/// Values stored for a single test in the history file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestValues {
    /// Total attempt count (unbounded).
    #[serde(rename = "n")]
    pub total_attempts: u64,

    /// Total failure count (unbounded).
    #[serde(rename = "f")]
    pub total_failures: u64,

    /// Run ID of the most recent run that included this test.
    pub last_run: String,

    /// Success reservoir: samples for passed attempts.
    #[serde(rename = "ok")]
    pub ok: Vec<CompactSample>,

    /// Failure reservoir: samples for failed attempts.
    #[serde(rename = "fail")]
    pub fail: Vec<CompactSample>,
}

/// Parse a single JSONL line into a TestRecord.
pub fn parse_line(line: &str) -> Result<TestRecord, HistoryError> {
    serde_json::from_str(line).map_err(|e| HistoryError::Parse(e.to_string()))
}

/// Serialize a TestRecord to a JSONL line (no trailing newline).
pub fn serialize_record(record: &TestRecord) -> Result<String, HistoryError> {
    serde_json::to_string(record).map_err(|e| HistoryError::Parse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compact_sample_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        let sample = CompactSample("aKx7".into(), 1712000000000, 2.1);
        let json = serde_json::to_string(&sample)?;
        assert_eq!(json, r#"["aKx7",1712000000000,2.1]"#);
        let parsed: CompactSample = serde_json::from_str(&json)?;
        assert_eq!(parsed.0, "aKx7");
        assert_eq!(parsed.1, 1712000000000);
        Ok(())
    }

    #[test]
    fn test_record_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        let record = TestRecord {
            key: ("offload.toml".into(), "tests/test.py::test_add".into()),
            values: TestValues {
                total_attempts: 47,
                total_failures: 3,
                last_run: "aKx7".into(),
                ok: vec![CompactSample("aKx7".into(), 1712000000000, 2.1)],
                fail: vec![CompactSample("Z9pQ".into(), 1711998000000, 2.3)],
            },
        };
        let json = serialize_record(&record)?;
        let parsed = parse_line(&json)?;
        assert_eq!(parsed.key.0, "offload.toml");
        assert_eq!(parsed.values.total_attempts, 47);
        assert_eq!(parsed.values.ok.len(), 1);
        assert_eq!(parsed.values.fail.len(), 1);
        Ok(())
    }

    #[test]
    fn test_json_format() -> Result<(), Box<dyn std::error::Error>> {
        let record = TestRecord {
            key: ("config.toml".into(), "test::foo".into()),
            values: TestValues {
                total_attempts: 10,
                total_failures: 2,
                last_run: "xyz".into(),
                ok: vec![],
                fail: vec![],
            },
        };
        let json = serialize_record(&record)?;
        // Verify compact field names
        assert!(json.contains(r#""k":"#));
        assert!(json.contains(r#""v":"#));
        assert!(json.contains(r#""n":10"#));
        assert!(json.contains(r#""f":2"#));
        Ok(())
    }
}
