//! Building human-readable summaries from trace records.

use crate::{DiagnosticSeverity, FileStatus, ReportConfig, TraceRecord};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Summary report over trace artifacts.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Report {
    /// Number of files checked.
    pub files_checked: usize,
    /// Files that passed.
    pub passed: usize,
    /// Files that failed.
    pub failed: usize,
    /// Files that timed out.
    pub timed_out: usize,
    /// Total parsed error diagnostics.
    pub error_count: usize,
    /// Total parsed warning diagnostics.
    pub warning_count: usize,
    /// Counts by normalized first-line diagnostic message.
    pub top_messages: BTreeMap<String, usize>,
}

/// Build a report from trace records.
#[must_use]
pub fn build_report(records: &[TraceRecord], _config: &ReportConfig) -> Report {
    let mut report = Report::default();

    for record in records {
        let TraceRecord::FileTrace(file_trace) = record;
        report.files_checked += 1;
        match file_trace.status {
            FileStatus::Passed => report.passed += 1,
            FileStatus::Failed | FileStatus::RunnerError => report.failed += 1,
            FileStatus::TimedOut => report.timed_out += 1,
        }

        for diagnostic in &file_trace.diagnostics {
            match diagnostic.severity {
                DiagnosticSeverity::Error => report.error_count += 1,
                DiagnosticSeverity::Warning => report.warning_count += 1,
                DiagnosticSeverity::Info | DiagnosticSeverity::Unknown => {}
            }
            let key = diagnostic
                .message
                .lines()
                .next()
                .unwrap_or("<empty diagnostic>")
                .trim()
                .to_owned();
            *report.top_messages.entry(key).or_insert(0) += 1;
        }
    }

    report
}
