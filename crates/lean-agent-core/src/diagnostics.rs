//! Parsing Lean/Lake diagnostic output.
//!
//! This starts intentionally conservative. The parser should preserve raw messages and only
//! structure fields it can infer reliably.

use crate::{Diagnostic, DiagnosticSeverity, GoalState};
use camino::Utf8PathBuf;
use regex::Regex;

/// Parse Lean diagnostics from stderr/stdout text.
///
/// TODO(parser-hardening): Replace this first-pass regex parser with a snapshot-tested parser
/// over a corpus of Lean 4 diagnostic fixtures from mathlib and your own repos.
#[must_use]
pub fn parse_lean_diagnostics(output: &str, include_warnings: bool) -> Vec<Diagnostic> {
    let Ok(header) = Regex::new(
        r"(?m)^(?P<file>[^:\n]+\.lean):(?P<line>\d+):(?P<column>\d+):\s*(?P<severity>error|warning|information):\s*(?P<message>.*)$",
    ) else {
        return Vec::new();
    };

    let mut diagnostics = Vec::new();
    let mut matches = header.captures_iter(output).peekable();

    while let Some(caps) = matches.next() {
        let Some(full_match) = caps.get(0) else {
            continue;
        };
        let message_start = full_match.start();
        let next_start = matches
            .peek()
            .and_then(|next| next.get(0))
            .map_or(output.len(), |m| m.start());
        let block = output[message_start..next_start].trim();

        let severity = match &caps["severity"] {
            "error" => DiagnosticSeverity::Error,
            "warning" => DiagnosticSeverity::Warning,
            "information" => DiagnosticSeverity::Info,
            _ => DiagnosticSeverity::Unknown,
        };

        if severity == DiagnosticSeverity::Warning && !include_warnings {
            continue;
        }

        let file = Some(Utf8PathBuf::from(&caps["file"]));
        let line = caps["line"].parse::<u32>().ok();
        let column = caps["column"].parse::<u32>().ok();
        let message = block.to_owned();
        let goal_state = extract_goal_state(block);

        diagnostics.push(Diagnostic {
            file,
            line,
            column,
            severity,
            message,
            goal_state,
        });
    }

    diagnostics
}

fn extract_goal_state(block: &str) -> Option<GoalState> {
    // TODO(goal-parser): Lean goal output has multiple shapes. Expand this with real fixtures.
    let marker = "unsolved goals";
    let idx = block.find(marker)?;
    let rest = block[idx + marker.len()..].trim();
    if rest.is_empty() {
        None
    } else {
        Some(GoalState(rest.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_unsolved_goal() {
        let raw = include_str!("../tests/fixtures/unsolved_goal.stderr");
        let diagnostics = parse_lean_diagnostics(raw, true);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
        assert!(diagnostics[0].goal_state.is_some());
    }
}
