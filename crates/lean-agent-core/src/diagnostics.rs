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

/// Recover a goal block from a diagnostic message the upstream parser did not
/// tag, for example a `Tactic ... failed` error or an unsynthesized placeholder
/// that still prints a turnstile.
///
/// The block is the turnstile (`⊢`) line plus the contiguous local-context lines
/// directly above it, stopping at the first blank line above.
#[must_use]
pub fn goal_from_message(message: &str) -> Option<GoalState> {
    let lines: Vec<&str> = message.lines().collect();
    let goal_idx = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with('⊢'))?;
    let mut start = goal_idx;
    while start > 0 && !lines[start - 1].trim().is_empty() {
        start -= 1;
    }
    let block = lines[start..=goal_idx].join("\n");
    let block = block.trim();
    (!block.is_empty()).then(|| GoalState(block.to_owned()))
}

/// Pick the most relevant goal state out of a set of diagnostics.
///
/// A diagnostic whose `goal_state` was already parsed (an `unsolved goals`
/// block) wins; otherwise the first diagnostic message carrying a turnstile is
/// recovered with [`goal_from_message`].
#[must_use]
pub fn recover_goal(diagnostics: &[Diagnostic]) -> Option<GoalState> {
    diagnostics
        .iter()
        .find_map(|d| d.goal_state.clone())
        .or_else(|| {
            diagnostics
                .iter()
                .find_map(|d| goal_from_message(&d.message))
        })
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
