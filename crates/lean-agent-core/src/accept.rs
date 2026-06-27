//! Accept predicate for replayed proof attempts.
//!
//! A passing `lake lean` exit code alone is gameable: an attempt can weaken the
//! statement it claims to prove, smuggle in `sorry`, or pin an extra axiom, and
//! still exit zero. This module turns "the file compiled" into "the file
//! compiled and the thing it proved is the thing we asked for" through a small
//! set of guards, each of which returns a typed rejection carrying its own
//! trace. It is not a proof of soundness: it raises the bar against the known
//! bypass classes below. See "Known limitations".
//!
//! Live guards (run on every passing compile):
//!
//! 1. STATEMENT-UNCHANGED. The declaration signature (everything up to the
//!    `:=` that opens the proof body) must be byte-identical before and after the
//!    edit. This guards against silent statement-weakening.
//! 2. AXIOM-WHITELIST. `#print axioms <decl>` must report a subset of
//!    {`propext`, `Classical.choice`, `Quot.sound`}. A `sorry` warning on the
//!    compile, or any axiom outside the set, is a rejection. The axiom set is
//!    read from a probe bracketed by per-run sentinels, so a top-level command
//!    in the edited file cannot forge the result, and an anonymous declaration
//!    (`example`) is aliased to a named probe rather than skipped. This guards
//!    against `sorry`/`admit` and extra-axiom passes.
//! 3. REVERSE-DEP. `lake build` of the edited module (and its direct importers
//!    when they are cheap to find) must succeed, so a weakened shared lemma
//!    cannot stay green on a stale olean.
//!
//! Guard 4 (NEGATIVE-CONTROL) is wired but stubbed. See [`check_negative_control`].
//!
//! Known limitations: these guards catch known bypass classes; they are not a
//! proof that no bypass exists. The axiom probe assumes the edit is a proof
//! body, so a replacement that introduces a top-level command (`#eval`,
//! `#print`, `import`, `set_option`, `macro`, `elab`, `open`) is refused at
//! patch time (see [`crate::patch`]); that restriction is what keeps the probe
//! trustworthy.

use crate::mine::module_name;
use crate::{
    Diagnostic, LeanFile, Provenance, Result, TraceConfig, detect_declaration, discover_lean_files,
    run_lean_file,
};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time;
use tracing::{debug, warn};
use uuid::Uuid;

/// Axioms a certified proof may depend on without weakening trust.
///
/// These are Lean's standard classical foundations. Anything else (notably
/// `sorryAx`) means the proof is not the proof we asked for.
pub const AXIOM_WHITELIST: [&str; 3] = ["propext", "Classical.choice", "Quot.sound"];

/// Most importer modules a single reverse-dependency guard will build.
const MAX_IMPORTERS: usize = 16;

/// Why the accept predicate refused to certify a passing compile.
///
/// Each variant carries the trace needed to see why the guard fired.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "guard", rename_all = "snake_case")]
pub enum RejectReason {
    /// The edit changed the declaration statement, not just the proof body.
    StatementChanged {
        /// Declaration whose statement changed.
        declaration: String,
        /// Signature before the edit.
        before: String,
        /// Signature after the edit.
        after: String,
    },
    /// The statement guard could not locate the declaration to compare.
    StatementUnverifiable {
        /// Why the comparison could not proceed.
        detail: String,
    },
    /// The compiled declaration depends on a non-whitelisted axiom.
    DisallowedAxiom {
        /// Declaration whose axioms were inspected.
        declaration: String,
        /// Axioms outside [`AXIOM_WHITELIST`] (for example `sorryAx`).
        offending: Vec<String>,
        /// Full axiom set Lean reported.
        axioms: Vec<String>,
    },
    /// The axiom set could not be read back from Lean.
    AxiomCheckFailed {
        /// Declaration whose axioms were being inspected.
        declaration: String,
        /// Why the check could not proceed.
        detail: String,
    },
    /// `lake build` of the edited module (or an importer) failed.
    ReverseDepFailed {
        /// Module that failed to build.
        module: String,
        /// First lines of the build failure.
        detail: String,
    },
    /// Negative control found the claim and its negation both pass (vacuous).
    VacuousClaim {
        /// Detail of the negative-control failure.
        detail: String,
    },
}

/// Outcome of one guard inside an [`AcceptReport`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GuardStatus {
    /// The guard ran and found nothing wrong.
    Passed,
    /// The guard did not run; `note` records why.
    Skipped {
        /// Human-readable reason the guard was not evaluated.
        note: String,
    },
    /// The guard refused the attempt.
    Rejected {
        /// Typed rejection with its trace.
        reason: RejectReason,
    },
}

/// Per-guard outcomes for one accept evaluation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AcceptReport {
    /// Guard 1: statement unchanged.
    pub statement: GuardStatus,
    /// Guard 2: axiom whitelist.
    pub axioms: GuardStatus,
    /// Guard 3: reverse dependency build.
    pub reverse_dep: GuardStatus,
    /// Guard 4: negative control (stubbed).
    pub negative_control: GuardStatus,
}

/// Result of running the accept predicate over one patched, passing compile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptOutcome {
    /// True only when every live guard passed.
    pub accepted: bool,
    /// Per-guard outcomes.
    pub report: AcceptReport,
    /// The first guard rejection, when one fired.
    pub reject_reason: Option<RejectReason>,
}

/// Negation manifest for the negative-control guard.
///
/// TODO(loop-phase): the loop must supply the formal negation of the claim so the
/// guard can compile it and require failure. Until then this type is the wiring
/// seam and [`check_negative_control`] is a documented no-op.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegativeControl {
    /// Lean source for the sibling declaration asserting the negation.
    pub negation_source: String,
    /// Name of the negation declaration.
    pub negation_name: String,
}

/// Everything the accept predicate needs for one attempt.
#[derive(Clone, Debug)]
pub struct AcceptRequest<'a> {
    /// Source-of-truth project root (holds the pre-edit file).
    pub lake_root: &'a Utf8Path,
    /// Patched workspace copy root (holds the post-edit file).
    pub workspace_root: &'a Utf8Path,
    /// Target file, relative to each root.
    pub target: &'a Utf8Path,
    /// One-based first line the edit was allowed to touch.
    pub edit_line: u32,
    /// Diagnostics from the patched compile (used to spot a `sorry` warning).
    pub patched_diagnostics: &'a [Diagnostic],
    /// Tooling provenance for any Lean run.
    pub provenance: &'a Provenance,
    /// Per-command timeout for axiom and build checks.
    pub timeout: Duration,
    /// Run the reverse-dependency guard (guard 3).
    pub run_reverse_dep: bool,
    /// Negative-control manifest, when the loop supplies one (guard 4).
    pub negative_control: Option<&'a NegativeControl>,
}

/// Run the live guards in order and return the first rejection, if any.
///
/// Guards short-circuit: once one rejects, the rest are marked skipped so the
/// report still shows why later guards did not run.
pub async fn evaluate(request: &AcceptRequest<'_>) -> AcceptOutcome {
    let not_eval = || GuardStatus::Skipped {
        note: "not evaluated; an earlier guard rejected".to_owned(),
    };

    let statement = check_statement(request);
    if let GuardStatus::Rejected { reason } = &statement {
        let reason = reason.clone();
        warn!(?reason, "accept predicate rejected on the statement guard");
        return AcceptOutcome {
            accepted: false,
            report: AcceptReport {
                statement,
                axioms: not_eval(),
                reverse_dep: not_eval(),
                negative_control: not_eval(),
            },
            reject_reason: Some(reason),
        };
    }

    let axioms = check_axioms(request).await;
    // The axiom guard must PASS for an attempt to be accepted. A rejection is
    // surfaced directly; a skip (which would otherwise leave `accepted` true) is
    // turned into a rejection so an unverifiable axiom set never counts as clean.
    let axiom_reject = match &axioms {
        GuardStatus::Passed => None,
        GuardStatus::Rejected { reason } => Some(reason.clone()),
        GuardStatus::Skipped { note } => Some(RejectReason::AxiomCheckFailed {
            declaration: "<unknown>".to_owned(),
            detail: format!("axiom guard did not run ({note}); refusing to accept"),
        }),
    };
    if let Some(reason) = axiom_reject {
        warn!(?reason, "accept predicate rejected on the axiom guard");
        return AcceptOutcome {
            accepted: false,
            report: AcceptReport {
                statement,
                axioms,
                reverse_dep: not_eval(),
                negative_control: not_eval(),
            },
            reject_reason: Some(reason),
        };
    }

    let reverse_dep = if request.run_reverse_dep {
        check_reverse_dep(request).await
    } else {
        GuardStatus::Skipped {
            note: "reverse-dependency guard disabled".to_owned(),
        }
    };
    if let GuardStatus::Rejected { reason } = &reverse_dep {
        let reason = reason.clone();
        warn!(
            ?reason,
            "accept predicate rejected on the reverse-dep guard"
        );
        return AcceptOutcome {
            accepted: false,
            report: AcceptReport {
                statement,
                axioms,
                reverse_dep,
                negative_control: not_eval(),
            },
            reject_reason: Some(reason),
        };
    }

    let negative_control = check_negative_control(request.negative_control);
    AcceptOutcome {
        accepted: true,
        report: AcceptReport {
            statement,
            axioms,
            reverse_dep,
            negative_control,
        },
        reject_reason: None,
    }
}

// ---------------------------------------------------------------------------
// Guard 1: statement unchanged
// ---------------------------------------------------------------------------

/// Result of comparing the enclosing declaration's statement before and after.
#[derive(Clone, Debug, Eq, PartialEq)]
enum StatementCheck {
    /// The signature is byte-identical before and after.
    Unchanged {
        /// Declaration label for the trace.
        declaration: String,
    },
    /// The signature differs.
    Changed {
        /// Declaration label for the trace.
        declaration: String,
        /// Signature before the edit.
        before: String,
        /// Signature after the edit.
        after: String,
    },
    /// The signature could not be extracted from one side.
    Unverifiable {
        /// Why extraction failed.
        detail: String,
    },
}

/// Guard 1. Read both copies of the target and compare the statement signature.
fn check_statement(request: &AcceptRequest<'_>) -> GuardStatus {
    let original_path = request.lake_root.join(request.target);
    let patched_path = request.workspace_root.join(request.target);

    let original = match std::fs::read_to_string(&original_path) {
        Ok(text) => text,
        Err(err) => {
            return GuardStatus::Rejected {
                reason: RejectReason::StatementUnverifiable {
                    detail: format!("reading original {original_path}: {err}"),
                },
            };
        }
    };
    let patched = match std::fs::read_to_string(&patched_path) {
        Ok(text) => text,
        Err(err) => {
            return GuardStatus::Rejected {
                reason: RejectReason::StatementUnverifiable {
                    detail: format!("reading patched {patched_path}: {err}"),
                },
            };
        }
    };

    match compare_statement(&original, &patched, request.edit_line) {
        StatementCheck::Unchanged { .. } => GuardStatus::Passed,
        StatementCheck::Changed {
            declaration,
            before,
            after,
        } => GuardStatus::Rejected {
            reason: RejectReason::StatementChanged {
                declaration,
                before,
                after,
            },
        },
        StatementCheck::Unverifiable { detail } => GuardStatus::Rejected {
            reason: RejectReason::StatementUnverifiable { detail },
        },
    }
}

/// Compare the signature of the declaration enclosing `edit_line` across copies.
fn compare_statement(original: &str, patched: &str, edit_line: u32) -> StatementCheck {
    let before = match enclosing_signature(original, edit_line) {
        Ok(pair) => pair,
        Err(detail) => {
            return StatementCheck::Unverifiable {
                detail: format!("original: {detail}"),
            };
        }
    };
    let after = match enclosing_signature(patched, edit_line) {
        Ok(pair) => pair,
        Err(detail) => {
            return StatementCheck::Unverifiable {
                detail: format!("patched: {detail}"),
            };
        }
    };

    if before.1 == after.1 {
        StatementCheck::Unchanged {
            declaration: before.0,
        }
    } else {
        StatementCheck::Changed {
            declaration: before.0,
            before: before.1,
            after: after.1,
        }
    }
}

/// Locate the declaration enclosing `edit_line` and return `(label, signature)`.
fn enclosing_signature(
    source: &str,
    edit_line: u32,
) -> std::result::Result<(String, String), String> {
    let lines: Vec<&str> = source.lines().collect();
    let idx = edit_line.saturating_sub(1) as usize;
    let decl = detect_declaration(&lines, idx)
        .ok_or_else(|| format!("no enclosing declaration at line {edit_line}"))?;
    let label = decl
        .name
        .clone()
        .map_or_else(|| decl.kind.clone(), |name| format!("{} {name}", decl.kind));
    Ok((label, statement_signature(&decl.source)))
}

/// Extract the statement signature: the declaration text up to the `:=` that
/// opens the proof body.
///
/// The split is on the first top-level `:=`, so a `:=` inside binders (a default
/// argument such as `(n : Nat := 0)`) or a string literal does not end the
/// signature. When no top-level `:=` is present the whole declaration is treated
/// as the signature, which is the conservative choice for `inductive`/`structure`
/// shapes that carry no proof body.
fn statement_signature(decl_source: &str) -> String {
    let bytes = decl_source.as_bytes();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let byte = bytes[i];
        if in_string {
            if byte == b'\\' {
                i += 2;
                continue;
            }
            if byte == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b':' if depth <= 0 && bytes.get(i + 1) == Some(&b'=') => {
                return decl_source[..i].trim_end().to_owned();
            }
            _ => {}
        }
        i += 1;
    }
    decl_source.trim_end().to_owned()
}

// ---------------------------------------------------------------------------
// Guard 2: axiom whitelist
// ---------------------------------------------------------------------------

/// Guard 2. Reject a `sorry` warning, then read `#print axioms <decl>` and
/// require the axiom set to sit inside [`AXIOM_WHITELIST`].
async fn check_axioms(request: &AcceptRequest<'_>) -> GuardStatus {
    let patched_path = request.workspace_root.join(request.target);
    let patched = match std::fs::read_to_string(&patched_path) {
        Ok(text) => text,
        Err(err) => {
            return GuardStatus::Rejected {
                reason: RejectReason::AxiomCheckFailed {
                    declaration: "<unknown>".to_owned(),
                    detail: format!("reading patched {patched_path}: {err}"),
                },
            };
        }
    };
    let lines: Vec<&str> = patched.lines().collect();
    let decl = detect_declaration(&lines, request.edit_line.saturating_sub(1) as usize);
    let label = decl.as_ref().map_or_else(
        || "<unknown>".to_owned(),
        |found| {
            found.name.clone().map_or_else(
                || found.kind.clone(),
                |name| format!("{} {name}", found.kind),
            )
        },
    );

    // A `sorry` leaves a warning even though the file exits zero. Catch it
    // directly, which also covers anonymous declarations the next step skips.
    if request
        .patched_diagnostics
        .iter()
        .any(|diagnostic| message_mentions_sorry(&diagnostic.message))
    {
        return GuardStatus::Rejected {
            reason: RejectReason::DisallowedAxiom {
                declaration: label,
                offending: vec!["sorryAx".to_owned()],
                axioms: vec!["sorryAx".to_owned()],
            },
        };
    }

    let Some(decl) = decl else {
        // Without a declaration there is nothing to read axioms from. Refuse,
        // rather than fall back to the sorry-scan alone, so an unreadable axiom
        // set never passes silently.
        return GuardStatus::Rejected {
            reason: RejectReason::AxiomCheckFailed {
                declaration: label,
                detail: "no enclosing declaration to read axioms from".to_owned(),
            },
        };
    };

    match print_axioms(request, &patched, &decl).await {
        Ok(axioms) => {
            let offending: Vec<String> = axioms
                .iter()
                .filter(|axiom| !AXIOM_WHITELIST.contains(&axiom.as_str()))
                .cloned()
                .collect();
            if offending.is_empty() {
                GuardStatus::Passed
            } else {
                GuardStatus::Rejected {
                    reason: RejectReason::DisallowedAxiom {
                        declaration: label,
                        offending,
                        axioms,
                    },
                }
            }
        }
        Err(detail) => GuardStatus::Rejected {
            reason: RejectReason::AxiomCheckFailed {
                declaration: label,
                detail,
            },
        },
    }
}

/// Compile a probe that prints the edited declaration's axioms between unique
/// sentinels, then parse only the text the probe itself emitted.
///
/// The patched file can hold attacker-controlled text, so the "does not depend
/// on any axioms" marker that [`parse_axioms`] reads is trusted only when it
/// appears between two per-run sentinels the probe emits. A top-level `#eval` in
/// the edited file prints outside that window and so cannot forge the result. An
/// anonymous declaration is aliased to a named `def` so it can be named in
/// `#print axioms`. The probe file is removed before returning.
async fn print_axioms(
    request: &AcceptRequest<'_>,
    patched: &str,
    decl: &crate::Declaration,
) -> std::result::Result<Vec<String>, String> {
    let nonce = Uuid::new_v4().simple().to_string();
    let begin = format!("LAP_AXIOMS_BEGIN_{nonce}");
    let end = format!("LAP_AXIOMS_END_{nonce}");

    // Name to print axioms of, plus any aliased declaration to append first.
    let (print_name, alias_decl) = match &decl.name {
        Some(name) => (name.clone(), None),
        None => {
            let alias = format!("__lap_probe_{nonce}");
            let source =
                alias_anonymous_decl(&decl.source, &decl.kind, &alias).ok_or_else(|| {
                    format!(
                        "could not build a named probe for anonymous `{}`",
                        decl.kind
                    )
                })?;
            (alias, Some(source))
        }
    };

    let probe_rel = probe_path(request.target);
    let probe_abs = request.workspace_root.join(&probe_rel);

    let mut probe_lines: Vec<String> = patched.lines().map(str::to_owned).collect();
    let insert_at = (decl.end_line as usize).min(probe_lines.len());

    let mut block: Vec<String> = Vec::new();
    if let Some(alias_decl) = &alias_decl {
        block.extend(alias_decl.lines().map(str::to_owned));
    }
    // The sentinel prints and `#print axioms` all write to stdout in source
    // order, so the axiom report lands between the two sentinels.
    block.push(format!("#eval IO.println {begin:?}"));
    block.push(format!("#print axioms {print_name}"));
    block.push(format!("#eval IO.println {end:?}"));
    for (offset, line) in block.into_iter().enumerate() {
        probe_lines.insert(insert_at + offset, line);
    }
    let mut probe_content = probe_lines.join("\n");
    probe_content.push('\n');

    if let Err(err) = std::fs::write(&probe_abs, &probe_content) {
        return Err(format!("writing axiom probe {probe_abs}: {err}"));
    }

    let mut config = TraceConfig::new(request.workspace_root.to_path_buf());
    config.timeout = request.timeout;
    config.include_warnings = true;
    config.keep_raw_output = true;
    let trace = run_lean_file(&config, request.provenance, LeanFile(probe_rel)).await;

    let _ = std::fs::remove_file(&probe_abs);

    let combined = format!(
        "{}\n{}",
        trace.stderr.unwrap_or_default(),
        trace.stdout.unwrap_or_default()
    );
    let window = slice_between(&combined, &begin, &end).ok_or_else(|| {
        format!("axiom probe produced no sentinel-bracketed output for `{print_name}`")
    })?;
    parse_axioms(window, &print_name)
        .ok_or_else(|| format!("could not read `#print axioms {print_name}` output"))
}

/// Return the text strictly between the first `begin` and the next following
/// `end`, or `None` when both sentinels are not present in that order.
fn slice_between<'a>(text: &'a str, begin: &str, end: &str) -> Option<&'a str> {
    let start = text.find(begin)? + begin.len();
    let rest = text.get(start..)?;
    let stop = rest.find(end)?;
    rest.get(..stop)
}

/// Rewrite an anonymous declaration into a named `def` so its axioms can be
/// printed.
///
/// The first standalone `kind` keyword in `source` becomes `def <alias>`; the
/// type and body are kept verbatim, so the aliased term elaborates to the same
/// proof and reports the same axioms. Returns `None` when the keyword is not
/// found, in which case the caller refuses the attempt.
fn alias_anonymous_decl(source: &str, kind: &str, alias: &str) -> Option<String> {
    let mut from = 0usize;
    while let Some(rel) = source.get(from..)?.find(kind) {
        let at = from + rel;
        let before_ok = at == 0
            || source
                .get(..at)
                .and_then(|prefix| prefix.chars().next_back())
                .is_some_and(char::is_whitespace);
        let after = at + kind.len();
        let after_ok = source
            .get(after..)
            .and_then(|rest| rest.chars().next())
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if before_ok && after_ok {
            let mut out = String::with_capacity(source.len() + alias.len() + 4);
            out.push_str(source.get(..at)?);
            out.push_str("def ");
            out.push_str(alias);
            out.push_str(source.get(after..)?);
            return Some(out);
        }
        from = after;
    }
    None
}

/// Probe-file path next to the target, marked so importer scans skip it.
fn probe_path(target: &Utf8Path) -> Utf8PathBuf {
    let stem = target.file_stem().unwrap_or("target");
    let file = format!("{stem}__lap_axioms.lean");
    match target.parent() {
        Some(parent) if !parent.as_str().is_empty() => parent.join(file),
        _ => Utf8PathBuf::from(file),
    }
}

/// Parse `#print axioms <name>` output for `name`'s axiom set.
///
/// Returns an empty vector for "does not depend on any axioms", the listed
/// axioms otherwise, or `None` when neither shape is present for `name`.
fn parse_axioms(output: &str, name: &str) -> Option<Vec<String>> {
    let none_marker = format!("'{name}' does not depend on any axioms");
    if output.contains(&none_marker) {
        return Some(Vec::new());
    }
    let some_marker = format!("'{name}' depends on axioms:");
    let after = output.find(&some_marker)? + some_marker.len();
    let rest = &output[after..];
    let open = rest.find('[')?;
    let close = rest[open..].find(']')? + open;
    let inner = &rest[open + 1..close];
    Some(
        inner
            .split(',')
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .collect(),
    )
}

/// True when a diagnostic message reports a `sorry` placeholder.
fn message_mentions_sorry(message: &str) -> bool {
    message.to_ascii_lowercase().contains("sorry")
}

// ---------------------------------------------------------------------------
// Guard 3: reverse dependency
// ---------------------------------------------------------------------------

/// Guard 3. Build the edited module from source, then any direct importers.
async fn check_reverse_dep(request: &AcceptRequest<'_>) -> GuardStatus {
    let module = module_for_target(request.workspace_root, request.target);

    match build_module(request.workspace_root, &module, request.timeout).await {
        Ok(true) => {}
        Ok(false) => {
            return GuardStatus::Rejected {
                reason: RejectReason::ReverseDepFailed {
                    module: module.clone(),
                    detail: "lake build reported failure".to_owned(),
                },
            };
        }
        Err(detail) => {
            return GuardStatus::Rejected {
                reason: RejectReason::ReverseDepFailed { module, detail },
            };
        }
    }

    for importer in find_importers(request.workspace_root, &module) {
        match build_module(request.workspace_root, &importer, request.timeout).await {
            Ok(true) => {}
            Ok(false) => {
                return GuardStatus::Rejected {
                    reason: RejectReason::ReverseDepFailed {
                        module: importer,
                        detail: "importer failed to build against the edited module".to_owned(),
                    },
                };
            }
            Err(detail) => {
                // An importer that cannot even be spawned is logged, not fatal:
                // the edited module already built cleanly above.
                warn!(%importer, %detail, "skipping importer that could not be built");
            }
        }
    }

    GuardStatus::Passed
}

/// Derive the dotted module name for `target` (relative to `root`), honoring the
/// lake library/executable source directories.
///
/// A `srcDir` layout maps to the real module name (`src/Demo/Basic.lean` becomes
/// `Demo.Basic`, not `src.Demo.Basic`). When no source directory matches, the
/// path is taken relative to the project root, which is correct for the default
/// root-relative layout.
fn module_for_target(root: &Utf8Path, target: &Utf8Path) -> String {
    for src_dir in lake_source_dirs(root) {
        if let Ok(stripped) = target.strip_prefix(Utf8Path::new(&src_dir)) {
            return module_name(stripped, Utf8Path::new("."));
        }
    }
    module_name(target, Utf8Path::new("."))
}

/// Source directories declared by the `lakefile.toml` libraries and executables.
///
/// Only `srcDir` values other than the project root are returned, longest first,
/// so the most specific source root is stripped before a module name is formed.
/// A missing or unparsable `lakefile.toml` yields an empty list and the caller
/// falls back to the project-root mapping.
fn lake_source_dirs(root: &Utf8Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.join("lakefile.toml")) else {
        return Vec::new();
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for table_key in ["lean_lib", "lean_exe"] {
        let Some(entries) = value.get(table_key).and_then(toml::Value::as_array) else {
            continue;
        };
        for entry in entries {
            if let Some(dir) = entry.get("srcDir").and_then(toml::Value::as_str) {
                let dir = dir.trim_matches('/');
                if !dir.is_empty() && dir != "." {
                    dirs.push(dir.to_owned());
                }
            }
        }
    }
    dirs.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    dirs.dedup();
    dirs
}

/// Run `lake build <module>` in `root` and report success, build failure, or a
/// spawn-level error.
async fn build_module(
    root: &Utf8Path,
    module: &str,
    timeout: Duration,
) -> std::result::Result<bool, String> {
    match run_capture("lake", &["build", module], root, timeout).await {
        Ok(output) => {
            if !output.success {
                debug!(%module, detail = %first_lines(&output.combined, 8), "lake build failed");
            }
            Ok(output.success)
        }
        Err(err) => Err(format!("lake build did not run: {err}")),
    }
}

/// Find modules under `root` whose source imports `module` directly.
fn find_importers(root: &Utf8Path, module: &str) -> Vec<String> {
    let Ok(files) = discover_lean_files(root, true) else {
        return Vec::new();
    };
    let exact = format!("import {module}");
    let prefix = format!("import {module} ");
    let mut importers = Vec::new();
    for file in files {
        let path = file.as_path();
        if path.as_str().contains("__lap_") {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        let imports_it = source.lines().any(|line| {
            let trimmed = line.trim();
            trimmed == exact || trimmed.starts_with(&prefix)
        });
        if imports_it {
            let relative = path.strip_prefix(root).unwrap_or(path);
            let importer = module_for_target(root, relative);
            if importer != module {
                importers.push(importer);
            }
        }
        if importers.len() >= MAX_IMPORTERS {
            break;
        }
    }
    importers.sort();
    importers.dedup();
    importers
}

// ---------------------------------------------------------------------------
// Guard 4: negative control (stub)
// ---------------------------------------------------------------------------

/// Guard 4. Compile the formal negation of the claim and require it to fail; if
/// both the claim and its negation pass, the claim is vacuous and is rejected.
///
/// TODO(loop-phase): this needs the claim manifest (the negation source and
/// name) that the loop has not produced yet. Until a [`NegativeControl`] is
/// supplied the guard is a documented no-op so guards 1-3 stay live on their own.
pub fn check_negative_control(control: Option<&NegativeControl>) -> GuardStatus {
    match control {
        None => GuardStatus::Skipped {
            note: "TODO(loop-phase): negative control needs the claim manifest".to_owned(),
        },
        Some(_) => GuardStatus::Skipped {
            note: "TODO(loop-phase): negation compile is not implemented yet".to_owned(),
        },
    }
}

// ---------------------------------------------------------------------------
// Process helper
// ---------------------------------------------------------------------------

/// Captured output of one external command.
struct CommandOutput {
    /// True when the process exited successfully.
    success: bool,
    /// Combined stderr and stdout, for the rejection trace.
    combined: String,
}

/// Spawn `program` with `args` in `cwd`, capturing output under a timeout.
///
/// A timeout is reported as a non-success result rather than an error, so a
/// build that hangs is treated as a build failure.
async fn run_capture(
    program: &str,
    args: &[&str],
    cwd: &Utf8Path,
    timeout: Duration,
) -> Result<CommandOutput> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command.spawn()?;
    match time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => {
            let output = result?;
            let combined = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
            Ok(CommandOutput {
                success: output.status.success(),
                combined,
            })
        }
        Err(_) => Ok(CommandOutput {
            success: false,
            combined: format!("timed out after {}s", timeout.as_secs()),
        }),
    }
}

/// First `count` lines of `text`, joined, for a compact trace.
fn first_lines(text: &str, count: usize) -> String {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .take(count)
        .collect::<Vec<_>>()
        .join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_splits_on_top_level_assign() {
        assert_eq!(
            statement_signature("theorem foo : 2 + 2 = 5 := by rfl"),
            "theorem foo : 2 + 2 = 5"
        );
    }

    #[test]
    fn signature_ignores_assign_inside_binders() {
        // The `:=` in the default argument must not end the signature.
        assert_eq!(
            statement_signature("def f (n : Nat := 0) : Nat := n"),
            "def f (n : Nat := 0) : Nat"
        );
    }

    #[test]
    fn signature_handles_multiline_body() {
        let source = "theorem bar : 1 + 1 = 2 := by\n  sorry";
        assert_eq!(statement_signature(source), "theorem bar : 1 + 1 = 2");
    }

    #[test]
    fn unchanged_body_keeps_statement() {
        let original = "theorem bar : 1 + 1 = 2 := by\n  sorry\n";
        let patched = "theorem bar : 1 + 1 = 2 := by\n  rfl\n";
        let check = compare_statement(original, patched, 2);
        assert!(matches!(check, StatementCheck::Unchanged { .. }));
    }

    #[test]
    fn weakened_statement_is_flagged() {
        let original = "theorem foo : 2 + 2 = 5 := by rfl\n";
        let patched = "theorem foo : 2 + 2 = 4 := by rfl\n";
        let check = compare_statement(original, patched, 1);
        assert!(matches!(check, StatementCheck::Changed { .. }));
        if let StatementCheck::Changed { before, after, .. } = check {
            assert_eq!(before, "theorem foo : 2 + 2 = 5");
            assert_eq!(after, "theorem foo : 2 + 2 = 4");
        }
    }

    #[test]
    fn missing_declaration_is_unverifiable() {
        let check = compare_statement("-- just a comment\n", "-- just a comment\n", 1);
        assert!(matches!(check, StatementCheck::Unverifiable { .. }));
    }

    #[test]
    fn parses_no_axioms() {
        let out = "'foo' does not depend on any axioms";
        assert_eq!(parse_axioms(out, "foo"), Some(Vec::new()));
    }

    #[test]
    fn parses_whitelisted_axioms() {
        let out = "'usesClassical' depends on axioms: [propext, Classical.choice, Quot.sound]";
        assert_eq!(
            parse_axioms(out, "usesClassical"),
            Some(vec![
                "propext".to_owned(),
                "Classical.choice".to_owned(),
                "Quot.sound".to_owned(),
            ])
        );
        let axioms = parse_axioms(out, "usesClassical").unwrap_or_default();
        assert!(axioms.iter().all(|a| AXIOM_WHITELIST.contains(&a.as_str())));
    }

    #[test]
    fn parses_sorry_axiom_as_offending() {
        let out = "'baz' depends on axioms: [sorryAx]";
        let axioms = parse_axioms(out, "baz").unwrap_or_default();
        assert_eq!(axioms, vec!["sorryAx".to_owned()]);
        assert!(
            axioms
                .iter()
                .any(|a| !AXIOM_WHITELIST.contains(&a.as_str()))
        );
    }

    #[test]
    fn parse_axioms_returns_none_for_other_decl() {
        let out = "'other' does not depend on any axioms";
        assert_eq!(parse_axioms(out, "foo"), None);
    }

    #[test]
    fn sorry_message_is_detected() {
        assert!(message_mentions_sorry("declaration uses `sorry`"));
        assert!(!message_mentions_sorry("unsolved goals"));
    }

    #[test]
    fn negative_control_is_a_documented_stub() {
        let status = check_negative_control(None);
        assert!(matches!(status, GuardStatus::Skipped { .. }));
        if let GuardStatus::Skipped { note } = status {
            assert!(note.contains("TODO(loop-phase)"));
        }
    }

    #[test]
    fn probe_path_sits_next_to_the_target() {
        assert_eq!(
            probe_path(Utf8Path::new("Demo.lean")).as_str(),
            "Demo__lap_axioms.lean"
        );
        assert_eq!(
            probe_path(Utf8Path::new("src/Demo.lean")).as_str(),
            "src/Demo__lap_axioms.lean"
        );
    }

    #[test]
    fn slice_between_reads_only_the_sentinel_window() {
        // A forged marker before BEGIN must not leak into the parsed window.
        let combined = "'foo' does not depend on any axioms\n\
             BEGIN\n'foo' depends on axioms: [evil]\nEND\ntrailing\n";
        let window = slice_between(combined, "BEGIN", "END").unwrap_or("");
        assert!(window.contains("[evil]"));
        assert!(!window.contains("does not depend"));
        assert_eq!(parse_axioms(window, "foo"), Some(vec!["evil".to_owned()]));
    }

    #[test]
    fn slice_between_needs_both_sentinels_in_order() {
        assert_eq!(slice_between("BEGIN only", "BEGIN", "END"), None);
        assert_eq!(slice_between("END before BEGIN", "BEGIN", "END"), None);
        assert_eq!(slice_between("neither", "BEGIN", "END"), None);
    }

    #[test]
    fn alias_rewrites_example_into_named_def() {
        assert_eq!(
            alias_anonymous_decl("example : 2 = 2 := by rfl", "example", "__p").unwrap_or_default(),
            "def __p : 2 = 2 := by rfl"
        );
    }

    #[test]
    fn alias_keeps_binders_and_swaps_only_the_keyword() {
        assert_eq!(
            alias_anonymous_decl("example (n : Nat) : n = n := by\n  rfl", "example", "__p")
                .unwrap_or_default(),
            "def __p (n : Nat) : n = n := by\n  rfl"
        );
    }

    #[test]
    fn alias_is_none_when_keyword_is_absent() {
        assert_eq!(
            alias_anonymous_decl("theorem t : True := trivial", "example", "__p"),
            None
        );
    }

    type BoxResult = std::result::Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn module_for_target_falls_back_to_root_relative() -> BoxResult {
        // No lakefile in this temp dir, so the path maps relative to the root.
        let dir = tempfile::TempDir::new()?;
        let root = Utf8Path::from_path(dir.path()).ok_or("non-UTF-8 temp path")?;
        assert_eq!(
            module_for_target(root, Utf8Path::new("Demo/Basic.lean")),
            "Demo.Basic"
        );
        Ok(())
    }

    #[test]
    fn lake_source_dirs_reads_srcdir_and_maps_module() -> BoxResult {
        let dir = tempfile::TempDir::new()?;
        let root = Utf8Path::from_path(dir.path()).ok_or("non-UTF-8 temp path")?;
        std::fs::write(
            root.join("lakefile.toml"),
            "name = \"demo\"\n\n[[lean_lib]]\nname = \"Demo\"\nsrcDir = \"src\"\n",
        )?;
        assert_eq!(lake_source_dirs(root), vec!["src".to_owned()]);
        assert_eq!(
            module_for_target(root, Utf8Path::new("src/Demo/Basic.lean")),
            "Demo.Basic"
        );
        Ok(())
    }
}
