//! Core primitives for Lean 4 tracing and theorem-agent evaluation.
//!
//! The crate is intentionally split into small modules so the CLI remains thin
//! and future users can reuse the library without depending on terminal UX.
//!
//! The library never spawns a model: a runner is an external process that
//! [`eval`] talks to over a line-oriented contract. Pure parsing helpers like
//! [`parse_lean_diagnostics`] run without a Lean toolchain, which makes them the
//! easiest entry point:
//!
//! ```
//! use lean_agent_core::{parse_lean_diagnostics, DiagnosticSeverity};
//!
//! let stderr = "Main.lean:3:0: error: unsolved goals\n⊢ 1 = 1";
//! let diagnostics = parse_lean_diagnostics(stderr, true);
//!
//! assert_eq!(diagnostics.len(), 1);
//! assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
//! assert_eq!(diagnostics[0].line, Some(3));
//! assert!(diagnostics[0].goal_state.is_some());
//! ```

#![warn(missing_docs)]

pub mod accept;
pub mod config;
pub mod context;
pub mod diagnostics;
pub mod discover;
pub mod error;
pub mod eval;
pub mod mine;
pub mod patch;
pub mod process;
pub mod replay;
pub mod report;
pub mod trace;
pub mod types;
pub mod workspace;
pub mod writer;

pub use accept::{
    AXIOM_WHITELIST, AcceptOutcome, AcceptReport, AcceptRequest, GuardStatus, NegativeControl,
    RejectReason, check_negative_control, evaluate,
};
pub use config::{FileConfig, ProjectConfig, ReportConfig, TraceConfig, TraceFileConfig};
pub use context::{
    ContextBundle, ContextOptions, ContextRequest, Declaration, SourceLine, SourceWindow,
    build_context, collect_imports, detect_declaration, gather_context, parse_file_line_spec,
};
pub use diagnostics::parse_lean_diagnostics;
pub use discover::discover_lean_files;
pub use error::{Error, Result};
pub use eval::{EvalOptions, EvalSummary, RunnerResponse, run_eval};
pub use mine::{
    AllowedEdit, MineKind, MineOptions, MineSummary, MineTask, TargetSpan, mine_errors,
    mine_placeholders, run_mine,
};
pub use patch::{AppliedPatch, SpanReplacement, apply_edits, apply_single_span};
pub use process::{LeanInvocation, LeanRunOutput, capture_provenance, run_lean_file};
pub use replay::{Attempt, ReplayOptions, ReplayResult, ReplayStatus, ReplaySummary, run_replay};
pub use report::{Report, build_report};
pub use trace::{TraceSummary, run_trace};
pub use types::{
    Diagnostic, DiagnosticSeverity, FileStatus, FileTrace, GoalState, LeanFile, Provenance,
    TraceRecord,
};
pub use workspace::{CopyOptions, Workspace};
pub use writer::{JsonlWriter, TraceWriter, write_jsonl};
