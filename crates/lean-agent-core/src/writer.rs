//! Trace artifact writers.

use crate::{Result, TraceRecord};
use camino::Utf8Path;
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, Write};

/// Streaming JSONL trace writer.
pub struct TraceWriter {
    inner: BufWriter<File>,
}

impl TraceWriter {
    /// Create a new writer at `path`.
    pub fn create(path: &Utf8Path) -> Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            inner: BufWriter::new(file),
        })
    }

    /// Write one trace record as one JSON line.
    pub fn write_record(&mut self, record: &TraceRecord) -> Result<()> {
        serde_json::to_writer(&mut self.inner, record)?;
        self.inner.write_all(b"\n")?;
        Ok(())
    }

    /// Flush buffered output.
    pub fn flush(&mut self) -> Result<()> {
        self.inner.flush()?;
        Ok(())
    }
}

/// Convenience writer for small record batches.
pub fn write_jsonl(path: &Utf8Path, records: &[TraceRecord]) -> Result<()> {
    let mut writer = TraceWriter::create(path)?;
    for record in records {
        writer.write_record(record)?;
    }
    writer.flush()
}

/// Streaming JSONL writer for any serializable record type.
///
/// Trace records carry a `record_type` tag; mined task records do not, so this
/// writer stays generic over the record type instead of taking [`TraceRecord`].
pub struct JsonlWriter {
    inner: BufWriter<File>,
}

impl JsonlWriter {
    /// Create a new writer at `path`, truncating any existing file.
    pub fn create(path: &Utf8Path) -> Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            inner: BufWriter::new(file),
        })
    }

    /// Write one record as a single JSON line.
    pub fn write_record<T: Serialize>(&mut self, record: &T) -> Result<()> {
        serde_json::to_writer(&mut self.inner, record)?;
        self.inner.write_all(b"\n")?;
        Ok(())
    }

    /// Flush buffered output to disk.
    pub fn flush(&mut self) -> Result<()> {
        self.inner.flush()?;
        Ok(())
    }
}
