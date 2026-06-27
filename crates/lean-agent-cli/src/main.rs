//! Command-line entrypoint for `lean-agent`.

use anyhow::Context;
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Parser, Subcommand, ValueEnum};
use lean_agent_core::{
    Attempt, ContextOptions, ContextRequest, EvalOptions, FileConfig, JsonlWriter, LeanFile,
    MineKind, MineOptions, MineTask, ReplayOptions, ReportConfig, TraceConfig, TraceRecord,
    TraceWriter, gather_context, parse_file_line_spec, run_eval, run_mine, run_replay, run_trace,
};
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "lean-agent")]
#[command(
    author,
    version,
    about = "Reproducible Lean 4 proof tracing and theorem-agent evaluation"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run Lean over files and emit structured JSONL traces.
    Trace(TraceArgs),
    /// Build a high-signal context bundle around one line of a Lean file.
    Context(ContextArgs),
    /// Mine replayable proof tasks (sorry/admit placeholders or errors) to JSONL.
    Mine(MineArgs),
    /// Summarize existing trace JSONL.
    Report(ReportArgs),
    /// Run theorem-agent benchmark tasks.
    Eval(EvalArgs),
    /// Replay previously generated proof attempts.
    Replay(ReplayArgs),
}

#[derive(Debug, Args)]
struct TraceArgs {
    /// Lean file or directory to trace. Falls back to `[project].source_roots` from config.
    path: Option<Utf8PathBuf>,

    /// Optional lean-agent.toml; CLI flags below override its values.
    #[arg(long)]
    config: Option<Utf8PathBuf>,

    /// Lake workspace root.
    #[arg(long)]
    lake_root: Option<Utf8PathBuf>,

    /// Search directories recursively.
    #[arg(short, long)]
    recursive: bool,

    /// Output JSONL path.
    #[arg(short, long, default_value = "lean-agent-trace.jsonl")]
    out: Utf8PathBuf,

    /// Timeout per Lean file in seconds.
    #[arg(long)]
    timeout: Option<u64>,

    /// Preserve raw stdout/stderr in JSONL. Use `--keep-raw-output=false` to override config.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    keep_raw_output: Option<bool>,

    /// Drop warnings from parsed diagnostics. Use `--no-warnings=false` to override config.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    no_warnings: Option<bool>,

    /// Emit only non-passing files. Use `--only-failures=false` to override config.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    only_failures: Option<bool>,

    /// Output format. Parquet is intentionally TODO.
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl)]
    format: OutputFormat,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    Jsonl,
    Parquet,
}

#[derive(Debug, Args)]
struct ContextArgs {
    /// Target line as FILE.lean:LINE (for example, src/Demo.lean:42).
    target: String,

    /// Lake workspace root used for the trace and provenance probes.
    #[arg(long, default_value = ".")]
    lake_root: Utf8PathBuf,

    /// Output path. Defaults to context.json, or context.md for markdown.
    #[arg(short, long)]
    out: Option<Utf8PathBuf>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = ContextFormat::Json)]
    format: ContextFormat,

    /// Source lines to include before the target line.
    #[arg(long, default_value_t = 8)]
    before: usize,

    /// Source lines to include after the target line.
    #[arg(long, default_value_t = 8)]
    after: usize,

    /// Skip running Lean; emit a static bundle with no diagnostics or goal state.
    #[arg(long)]
    no_trace: bool,

    /// Timeout for the Lean trace in seconds.
    #[arg(long, default_value_t = 60)]
    timeout: u64,

    /// Drop warning diagnostics from the bundle.
    #[arg(long)]
    no_warnings: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ContextFormat {
    Json,
    Markdown,
}

#[derive(Debug, Args)]
struct MineArgs {
    /// File or directory to mine. Defaults to the current directory.
    #[arg(default_value = ".")]
    path: Utf8PathBuf,

    /// Lake workspace root, also used to derive module names.
    #[arg(long, default_value = ".")]
    lake_root: Utf8PathBuf,

    /// What to mine.
    #[arg(long, value_enum, default_value_t = MineKindArg::Sorry)]
    kind: MineKindArg,

    /// Output JSONL path.
    #[arg(short, long, default_value = "tasks.jsonl")]
    out: Utf8PathBuf,

    /// Search directories recursively.
    #[arg(short, long)]
    recursive: bool,

    /// Project name stamped onto each task. Defaults to the lake-root directory name.
    #[arg(long)]
    project: Option<String>,

    /// Per-file Lean timeout in seconds (error mining only).
    #[arg(long, default_value_t = 60)]
    timeout: u64,

    /// Extra path substrings to skip during discovery (.lake/ is always skipped).
    #[arg(long)]
    exclude: Vec<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum MineKindArg {
    Sorry,
    Admit,
    Error,
}

impl From<MineKindArg> for MineKind {
    fn from(value: MineKindArg) -> Self {
        match value {
            MineKindArg::Sorry => Self::Sorry,
            MineKindArg::Admit => Self::Admit,
            MineKindArg::Error => Self::Error,
        }
    }
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// Input JSONL trace path.
    input: Utf8PathBuf,
}

#[derive(Debug, Args)]
struct EvalArgs {
    /// Mined task JSONL (the output of `lean-agent mine`).
    tasks: Utf8PathBuf,

    /// Runner executable/script speaking the line-oriented process contract.
    #[arg(long)]
    runner: Utf8PathBuf,

    /// Lake workspace root, forwarded to the runner as LEAN_AGENT_LAKE_ROOT.
    #[arg(long, default_value = ".")]
    lake_root: Utf8PathBuf,

    /// Output JSONL path for replayable attempts.
    #[arg(short, long, default_value = "attempts.jsonl")]
    out: Utf8PathBuf,

    /// Per-task reply timeout in seconds.
    #[arg(long, default_value_t = 120)]
    timeout: u64,
}

#[derive(Debug, Args)]
struct ReplayArgs {
    /// Attempts JSONL: one bounded edit per line.
    attempts: Utf8PathBuf,

    /// Lake workspace root, copied per attempt.
    #[arg(long, default_value = ".")]
    lake_root: Utf8PathBuf,

    /// Output JSONL path for result records.
    #[arg(short, long, default_value = "results.jsonl")]
    out: Utf8PathBuf,

    /// Per-compile timeout in seconds.
    #[arg(long, default_value_t = 60)]
    timeout: u64,

    /// Keep each workspace copy on disk instead of deleting it.
    #[arg(long)]
    keep_workdir: bool,

    /// Allow an attempt to edit more than one file.
    #[arg(long)]
    allow_multi_file: bool,

    /// Skip the unpatched baseline compile (new-error and regression scoring).
    #[arg(long)]
    no_baseline: bool,

    /// Skip the reverse-dependency accept guard (the `lake build` of the module).
    #[arg(long)]
    no_reverse_dep: bool,

    /// Skip the best-effort `lake exe cache get` for mathlib-backed projects.
    #[arg(long)]
    no_cache_get: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    let cli = Cli::parse();

    match cli.command {
        Command::Trace(args) => trace(args).await,
        Command::Context(args) => context(args).await,
        Command::Mine(args) => mine(args).await,
        Command::Report(args) => report(args),
        Command::Eval(args) => eval(args).await,
        Command::Replay(args) => replay(args).await,
    }
}

async fn mine(args: MineArgs) -> anyhow::Result<()> {
    let kind = MineKind::from(args.kind);
    let project = args
        .project
        .clone()
        .unwrap_or_else(|| derive_project_name(&args.lake_root));

    let mut exclude = vec![".lake/".to_owned()];
    exclude.extend(args.exclude.clone());

    let options = MineOptions {
        kind,
        project,
        lake_root: args.lake_root.clone(),
        recursive: args.recursive,
        timeout: Duration::from_secs(args.timeout),
        exclude,
    };
    let roots = vec![args.path.clone()];

    let mut writer = JsonlWriter::create(&args.out)
        .with_context(|| format!("creating output file {}", args.out))?;
    let summary = run_mine(&options, &roots, &mut writer)
        .await
        .context("running mine")?;

    info!(
        files_scanned = summary.files_scanned,
        tasks_written = summary.tasks_written,
        kind = ?args.kind,
        out = %args.out,
        "mine complete"
    );
    Ok(())
}

/// Derive a project name from the lake-root directory name.
fn derive_project_name(lake_root: &Utf8Path) -> String {
    if let Some(name) = lake_root.file_name() {
        if !name.is_empty() {
            return name.to_owned();
        }
    }
    std::fs::canonicalize(lake_root)
        .ok()
        .and_then(|path| Utf8PathBuf::from_path_buf(path).ok())
        .and_then(|path| path.file_name().map(str::to_owned))
        .unwrap_or_else(|| "project".to_owned())
}

async fn context(args: ContextArgs) -> anyhow::Result<()> {
    let (path, line) = parse_file_line_spec(&args.target)
        .with_context(|| format!("parsing target {}", args.target))?;
    let file = LeanFile::new(path).context("target must be a .lean file")?;

    let request = ContextRequest {
        file,
        line,
        before: args.before,
        after: args.after,
    };
    let options = ContextOptions {
        run_trace: !args.no_trace,
        lake_root: args.lake_root.clone(),
        timeout: Duration::from_secs(args.timeout),
        include_warnings: !args.no_warnings,
    };

    let bundle = gather_context(&request, &options)
        .await
        .context("gathering context")?;

    let out = args.out.unwrap_or_else(|| match args.format {
        ContextFormat::Json => Utf8PathBuf::from("context.json"),
        ContextFormat::Markdown => Utf8PathBuf::from("context.md"),
    });
    let rendered = match args.format {
        ContextFormat::Json => serde_json::to_string_pretty(&bundle)?,
        ContextFormat::Markdown => bundle.to_markdown(),
    };
    std::fs::write(&out, &rendered).with_context(|| format!("writing {out}"))?;

    info!(
        out = %out,
        line = bundle.line,
        declaration = bundle.declaration.as_ref().map_or("none", |d| d.kind.as_str()),
        diagnostics = bundle.diagnostics.len(),
        "context bundle written"
    );
    Ok(())
}

async fn trace(args: TraceArgs) -> anyhow::Result<()> {
    match args.format {
        OutputFormat::Jsonl => {}
        OutputFormat::Parquet => anyhow::bail!("Parquet output is TODO for v0.3"),
    }

    let file_config = match &args.config {
        Some(path) => FileConfig::load(path).with_context(|| format!("loading config {path}"))?,
        None => FileConfig::default(),
    };
    if let Some(name) = &file_config.project.name {
        info!(project = %name, "loaded project config");
    }

    // CLI flags take precedence over config, which takes precedence over hardcoded defaults.
    let lake_root = args
        .lake_root
        .or(file_config.project.lake_root.clone())
        .unwrap_or_else(|| Utf8PathBuf::from("."));
    let timeout_secs = args
        .timeout
        .or(file_config.trace.timeout_secs)
        .unwrap_or(60);
    let keep_raw_output = args
        .keep_raw_output
        .or(file_config.trace.keep_raw_output)
        .unwrap_or(false);
    let include_warnings = match args.no_warnings {
        Some(no_warnings) => !no_warnings,
        None => file_config.trace.include_warnings.unwrap_or(true),
    };
    let only_failures = args
        .only_failures
        .or(file_config.trace.only_failures)
        .unwrap_or(false);

    let config = TraceConfig {
        lake_root: lake_root.clone(),
        recursive: args.recursive,
        timeout: Duration::from_secs(timeout_secs),
        keep_raw_output,
        include_warnings,
        include_passes: !only_failures,
        exclude: file_config.project.exclude.clone(),
    };

    let roots = resolve_roots(args.path, &lake_root, &file_config)?;

    let mut writer = TraceWriter::create(&args.out)
        .with_context(|| format!("creating output file {}", args.out))?;

    let summary = run_trace(&config, &roots, &mut writer)
        .await
        .context("running trace")?;
    info!(
        files_run = summary.files_run,
        records_written = summary.records_written,
        non_passing = summary.non_passing,
        out = %args.out,
        "trace complete"
    );
    Ok(())
}

/// Resolve trace targets: an explicit path wins, else config source_roots.
fn resolve_roots(
    path: Option<Utf8PathBuf>,
    lake_root: &Utf8Path,
    file_config: &FileConfig,
) -> anyhow::Result<Vec<Utf8PathBuf>> {
    if let Some(path) = path {
        return Ok(vec![path]);
    }
    if !file_config.project.source_roots.is_empty() {
        return Ok(file_config
            .project
            .source_roots
            .iter()
            .map(|root| {
                if root.is_absolute() {
                    root.clone()
                } else {
                    lake_root.join(root)
                }
            })
            .collect());
    }
    anyhow::bail!("no PATH given and no [project].source_roots in config")
}

fn report(args: ReportArgs) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(&args.input)
        .with_context(|| format!("reading trace file {}", args.input))?;
    let mut records = Vec::new();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        records.push(serde_json::from_str::<TraceRecord>(line)?);
    }
    let report = lean_agent_core::build_report(&records, &ReportConfig::default());
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn eval(args: EvalArgs) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(&args.tasks)
        .with_context(|| format!("reading tasks file {}", args.tasks))?;

    let mut tasks = Vec::new();
    let mut skipped = 0usize;
    for (index, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<MineTask>(trimmed) {
            Ok(task) => tasks.push(task),
            Err(err) => {
                skipped += 1;
                warn!(line = index + 1, error = %err, "skipping malformed task");
            }
        }
    }

    let options = EvalOptions {
        runner: args.runner.clone(),
        lake_root: args.lake_root.clone(),
        timeout: Duration::from_secs(args.timeout),
    };

    let mut writer = JsonlWriter::create(&args.out)
        .with_context(|| format!("creating output file {}", args.out))?;
    let summary = run_eval(&options, &tasks, &mut writer)
        .await
        .context("running eval")?;

    info!(
        tasks_read = summary.tasks_read,
        attempts_written = summary.attempts_written,
        runner_errors = summary.runner_errors,
        id_mismatches = summary.id_mismatches,
        skipped,
        out = %args.out,
        "eval complete"
    );
    Ok(())
}

async fn replay(args: ReplayArgs) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(&args.attempts)
        .with_context(|| format!("reading attempts file {}", args.attempts))?;

    let mut attempts = Vec::new();
    let mut skipped = 0usize;
    for (index, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Attempt>(trimmed) {
            Ok(attempt) => attempts.push(attempt),
            Err(err) => {
                skipped += 1;
                warn!(line = index + 1, error = %err, "skipping malformed attempt");
            }
        }
    }

    let options = ReplayOptions {
        lake_root: args.lake_root.clone(),
        timeout: Duration::from_secs(args.timeout),
        keep_workdir: args.keep_workdir,
        allow_multi_file: args.allow_multi_file,
        compute_baseline: !args.no_baseline,
        reverse_dep: !args.no_reverse_dep,
        cache_get: !args.no_cache_get,
    };

    let mut writer = JsonlWriter::create(&args.out)
        .with_context(|| format!("creating output file {}", args.out))?;
    let summary = run_replay(&options, &attempts, &mut writer)
        .await
        .context("running replay")?;

    info!(
        attempts = summary.attempts,
        compiled_pass = summary.compiled_pass,
        rejected = summary.rejected,
        compiled_fail = summary.compiled_fail,
        patch_refused = summary.patch_refused,
        errored = summary.errored,
        skipped,
        out = %args.out,
        "replay complete"
    );
    Ok(())
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
