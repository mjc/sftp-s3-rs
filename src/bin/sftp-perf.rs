use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const DEFAULT_RUSSH_REPO: &str = "/home/mjc/projects/russh";
const DEFAULT_RUSSH_SFTP_REPO: &str = "/home/mjc/projects/russh-sftp";
const DEFAULT_MATRIX_RUSSH_REFS: [&str; 2] = ["main", "mjc/own-inbound-channel-payloads"];
const DEFAULT_MATRIX_SFTP_REFS: [&str; 2] = ["master", "deserialize-bytes-optimization"];

#[derive(Debug, Parser)]
#[command(name = "sftp-perf")]
#[command(about = "Benchmark and profile SFTP server builds")]
struct Cli {
    #[command(subcommand)]
    command: PerfCommand,
}

#[derive(Debug, Subcommand)]
enum PerfCommand {
    /// Benchmark the current checkout with its configured dependencies
    Current(CurrentArgs),
    /// Benchmark a local stack using specific russh/russh-sftp refs
    LocalStack(LocalStackArgs),
    /// Benchmark a russh × russh-sftp version matrix
    Matrix(MatrixArgs),
    /// Record a perf profile for the selected workload
    Profile(ProfileArgs),
    /// Record a heaptrack profile for the selected workload
    Heaptrack(ProfileArgs),
    /// List recent benchmark and profiling runs
    List(ListArgs),
    /// Show details for a specific run
    Show(RunRefArgs),
    /// Mark a run invalid for future comparisons
    MarkInvalid(MarkInvalidArgs),
    /// Mark a run valid for future comparisons
    MarkValid(RunRefArgs),
}

#[derive(Debug, Clone, Args)]
struct CommonArgs {
    /// Client implementation to drive the benchmark
    #[arg(long, value_enum, default_value_t = ClientKind::Bench)]
    client: ClientKind,

    /// Operations to run
    #[arg(long, value_enum, default_value_t = OperationSelection::All)]
    operation: OperationSelection,

    /// Comma-separated MiB sizes to benchmark
    #[arg(long, value_delimiter = ',', default_values_t = vec![1024_u64, 10240_u64])]
    sizes: Vec<u64>,

    /// Number of measured runs per operation/size
    #[arg(long)]
    runs: Option<u32>,

    /// Number of warmup runs per operation/size
    #[arg(long)]
    warmup: Option<u32>,

    /// Preferred cipher list
    #[arg(long)]
    ciphers: Option<String>,

    /// Benchmark backend to use
    #[arg(long, value_enum, default_value_t = BackendKind::Benchmark)]
    backend: BackendKind,

    /// Optional label suffix for the run directory
    #[arg(long)]
    label: Option<String>,

    /// Add a note to the stored run metadata
    #[arg(long)]
    note: Vec<String>,

    /// Chunk size passed to the Rust benchmark client
    #[arg(long, default_value = "64KiB")]
    chunk_size: String,

    /// Disable sccache even if present
    #[arg(long)]
    no_sccache: bool,
}

#[derive(Debug, Clone, Args)]
struct CurrentArgs {
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Debug, Clone, Args)]
struct LocalStackArgs {
    #[command(flatten)]
    common: CommonArgs,

    #[arg(long, default_value = DEFAULT_RUSSH_REPO)]
    russh_repo: PathBuf,

    #[arg(long, default_value = "mjc/own-inbound-channel-payloads")]
    russh_ref: String,

    #[arg(long, default_value = DEFAULT_RUSSH_SFTP_REPO)]
    russh_sftp_repo: PathBuf,

    #[arg(long, default_value = "deserialize-bytes-optimization")]
    russh_sftp_ref: String,

    #[arg(long, default_value = "HEAD")]
    sftp_s3_ref: String,

    /// Extra server features to enable when building
    #[arg(long, value_delimiter = ',')]
    server_features: Vec<String>,
}

#[derive(Debug, Clone, Args)]
struct MatrixArgs {
    #[command(flatten)]
    common: CommonArgs,

    #[arg(long, default_value = DEFAULT_RUSSH_REPO)]
    russh_repo: PathBuf,

    #[arg(long, default_value = DEFAULT_RUSSH_SFTP_REPO)]
    russh_sftp_repo: PathBuf,

    #[arg(long, value_delimiter = ',', default_values_t = DEFAULT_MATRIX_RUSSH_REFS.iter().map(|s| s.to_string()).collect::<Vec<_>>())]
    russh_refs: Vec<String>,

    #[arg(long, value_delimiter = ',', default_values_t = DEFAULT_MATRIX_SFTP_REFS.iter().map(|s| s.to_string()).collect::<Vec<_>>())]
    russh_sftp_refs: Vec<String>,

    #[arg(long, default_value = "current-current")]
    baseline: String,
}

#[derive(Debug, Clone, Args)]
struct ProfileArgs {
    #[command(flatten)]
    common: CommonArgs,

    #[arg(long, default_value = DEFAULT_RUSSH_REPO)]
    russh_repo: PathBuf,

    #[arg(long)]
    russh_ref: Option<String>,

    #[arg(long, default_value = DEFAULT_RUSSH_SFTP_REPO)]
    russh_sftp_repo: PathBuf,

    #[arg(long)]
    russh_sftp_ref: Option<String>,

    #[arg(long, default_value = "HEAD")]
    sftp_s3_ref: String,

    #[arg(long, value_delimiter = ',')]
    server_features: Vec<String>,

    #[arg(long, default_value_t = 999)]
    perf_frequency: u32,
}

#[derive(Debug, Clone, Args)]
struct ListArgs {
    /// Maximum number of runs to show
    #[arg(long, default_value_t = 20)]
    limit: usize,

    /// Include runs already marked invalid
    #[arg(long)]
    all: bool,
}

#[derive(Debug, Clone, Args)]
struct RunRefArgs {
    /// Run id under benchmark_results/runs/
    run_id: String,
}

#[derive(Debug, Clone, Args)]
struct MarkInvalidArgs {
    /// Run id under benchmark_results/runs/
    run_id: String,

    /// Why this run should be ignored in comparisons
    #[arg(long)]
    reason: String,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ClientKind {
    #[value(alias = "rust")]
    Bench,
    Openssh,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OperationSelection {
    Upload,
    Download,
    Roundtrip,
    All,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BackendKind {
    Benchmark,
    Memory,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum OperationKind {
    Upload,
    Download,
    Roundtrip,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProfileKind {
    Perf,
    Heaptrack,
}

#[derive(Debug)]
struct AppContext {
    repo_root: PathBuf,
    cache_root: PathBuf,
    results_root: PathBuf,
    sccache_bin: Option<PathBuf>,
    keys: KeyMaterial,
}

#[derive(Debug, Clone)]
struct KeyMaterial {
    private_key: PathBuf,
    authorized_keys: PathBuf,
}

#[derive(Debug, Clone)]
struct TargetSpec {
    label: String,
    source: SourceSpec,
    features: Vec<String>,
}

#[derive(Debug, Clone)]
enum SourceSpec {
    Current {
        snapshot: String,
    },
    LocalStack {
        sftp_s3_ref: String,
        russh_repo: PathBuf,
        russh_ref: String,
        russh_sftp_repo: PathBuf,
        russh_sftp_ref: String,
    },
}

#[derive(Debug, Clone)]
struct BuildPlan {
    label: String,
    manifest_source: PathBuf,
    build_id: String,
    features: Vec<String>,
    snapshot: String,
    russh_ref: Option<String>,
    russh_sftp_ref: Option<String>,
    source_mode: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BenchClientJson {
    operation: OperationKind,
    bytes: u64,
    iterations: u64,
    run_id: String,
    results: Vec<BenchClientIteration>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BenchClientIteration {
    upload_seconds: Option<f64>,
    download_seconds: Option<f64>,
    total_seconds: f64,
}

#[derive(Debug, Serialize, Deserialize)]
struct RunManifest {
    run_id: String,
    timestamp_unix: u64,
    mode: String,
    client: ClientKind,
    backend: BackendKind,
    ciphers: Option<String>,
    sizes_mb: Vec<u64>,
    operations: Vec<OperationKind>,
    comparison_key: String,
    profile_mode: Option<ProfileKind>,
    matrix_baseline: Option<String>,
    #[serde(default = "default_true")]
    valid_for_comparison: bool,
    #[serde(default)]
    invalid_reason: Option<String>,
    #[serde(default)]
    notes: Vec<String>,
    machine: MachineMetadata,
    targets: Vec<TargetManifest>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TargetManifest {
    label: String,
    source_mode: String,
    snapshot: String,
    russh_ref: Option<String>,
    russh_sftp_ref: Option<String>,
    features: Vec<String>,
    manifest_source: String,
    build_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct MachineMetadata {
    os: String,
    arch: String,
    hostname: String,
    #[serde(default = "unknown_string")]
    cpu: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RunResults {
    run_id: String,
    comparison_key: String,
    records: Vec<MeasurementRecord>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct MeasurementRecord {
    target_label: String,
    operation: OperationKind,
    size_mb: u64,
    bytes_per_iteration: u64,
    iterations_seconds: Vec<f64>,
    mean_seconds: f64,
    stddev_seconds: f64,
    min_seconds: f64,
    max_seconds: f64,
    throughput_mib_per_second: f64,
}

#[derive(Debug, Clone)]
struct PreviousRun {
    run_id: String,
    results: RunResults,
}

struct RunRecord {
    manifest: RunManifest,
}

fn default_true() -> bool {
    true
}

fn unknown_string() -> String {
    "unknown".to_string()
}

fn main() -> Result<(), BoxError> {
    let cli = Cli::parse();
    let ctx = AppContext::new()?;

    match cli.command {
        PerfCommand::Current(args) => run_mode(
            &ctx,
            "current",
            args.common,
            vec![TargetSpec {
                label: "current".to_string(),
                source: SourceSpec::Current {
                    snapshot: snapshot_id(&ctx.repo_root)?,
                },
                features: Vec::new(),
            }],
            None,
            None,
            None,
        ),
        PerfCommand::LocalStack(args) => run_mode(
            &ctx,
            "local-stack",
            args.common,
            vec![TargetSpec {
                label: "local-stack".to_string(),
                source: SourceSpec::LocalStack {
                    sftp_s3_ref: args.sftp_s3_ref,
                    russh_repo: args.russh_repo,
                    russh_ref: args.russh_ref,
                    russh_sftp_repo: args.russh_sftp_repo,
                    russh_sftp_ref: args.russh_sftp_ref,
                },
                features: args.server_features,
            }],
            None,
            None,
            None,
        ),
        PerfCommand::Matrix(args) => run_matrix(&ctx, args),
        PerfCommand::Profile(args) => run_profile(&ctx, args, ProfileKind::Perf),
        PerfCommand::Heaptrack(args) => run_profile(&ctx, args, ProfileKind::Heaptrack),
        PerfCommand::List(args) => list_runs(&ctx, args),
        PerfCommand::Show(args) => show_run(&ctx, &args.run_id),
        PerfCommand::MarkInvalid(args) => mark_run_invalid(&ctx, &args.run_id, &args.reason),
        PerfCommand::MarkValid(args) => mark_run_valid(&ctx, &args.run_id),
    }
}

impl AppContext {
    fn new() -> Result<Self, BoxError> {
        let repo_root = repo_root()?;
        let cache_root = repo_root.join(".perf-cache");
        let results_root = repo_root.join("benchmark_results").join("runs");
        fs::create_dir_all(&cache_root)?;
        fs::create_dir_all(&results_root)?;
        let keys = ensure_key_material(&cache_root)?;
        Ok(Self {
            repo_root,
            cache_root,
            results_root,
            sccache_bin: command_path("sccache"),
            keys,
        })
    }
}

fn run_profile(
    ctx: &AppContext,
    args: ProfileArgs,
    profile_kind: ProfileKind,
) -> Result<(), BoxError> {
    let target = if args.russh_ref.is_some()
        || args.russh_sftp_ref.is_some()
        || !args.server_features.is_empty()
        || args.sftp_s3_ref != "HEAD"
    {
        TargetSpec {
            label: profile_label(&args),
            source: SourceSpec::LocalStack {
                sftp_s3_ref: args.sftp_s3_ref,
                russh_repo: args.russh_repo,
                russh_ref: args
                    .russh_ref
                    .unwrap_or_else(|| "mjc/own-inbound-channel-payloads".to_string()),
                russh_sftp_repo: args.russh_sftp_repo,
                russh_sftp_ref: args
                    .russh_sftp_ref
                    .unwrap_or_else(|| "deserialize-bytes-optimization".to_string()),
            },
            features: args.server_features,
        }
    } else {
        TargetSpec {
            label: "current".to_string(),
            source: SourceSpec::Current {
                snapshot: snapshot_id(&ctx.repo_root)?,
            },
            features: Vec::new(),
        }
    };

    run_mode(
        ctx,
        match profile_kind {
            ProfileKind::Perf => "profile",
            ProfileKind::Heaptrack => "heaptrack",
        },
        args.common,
        vec![target],
        Some(profile_kind),
        Some(args.perf_frequency),
        None,
    )
}

fn run_matrix(ctx: &AppContext, args: MatrixArgs) -> Result<(), BoxError> {
    let baseline = args.baseline.clone();
    let mut targets = Vec::new();
    for russh_ref in &args.russh_refs {
        for sftp_ref in &args.russh_sftp_refs {
            let russh_label = matrix_label(russh_ref);
            let sftp_label = matrix_label(sftp_ref);
            let features = vec!["benchmark-matrix-compat".to_string()];
            targets.push(TargetSpec {
                label: format!("{russh_label}-{sftp_label}"),
                source: SourceSpec::LocalStack {
                    sftp_s3_ref: "HEAD".to_string(),
                    russh_repo: args.russh_repo.clone(),
                    russh_ref: russh_ref.clone(),
                    russh_sftp_repo: args.russh_sftp_repo.clone(),
                    russh_sftp_ref: sftp_ref.clone(),
                },
                features,
            });
        }
    }
    run_mode(
        ctx,
        "matrix",
        args.common,
        targets,
        None,
        None,
        Some(baseline),
    )
}

fn run_mode(
    ctx: &AppContext,
    mode: &str,
    common: CommonArgs,
    targets: Vec<TargetSpec>,
    profile_mode: Option<ProfileKind>,
    perf_frequency: Option<u32>,
    matrix_baseline: Option<String>,
) -> Result<(), BoxError> {
    ensure_sizes(&common.sizes)?;
    let run_id = build_run_id(mode, common.label.as_deref());
    let run_dir = ctx.results_root.join(&run_id);
    fs::create_dir_all(&run_dir)?;

    let operations = selected_operations(common.operation);
    let plans = targets
        .into_iter()
        .map(|target| make_build_plan(ctx, &target))
        .collect::<Result<Vec<_>, _>>()?;

    let comparison_key = make_comparison_key(
        mode,
        common.client,
        common.backend,
        common.ciphers.as_deref(),
        &common.sizes,
        &operations,
        &plans,
        profile_mode,
    );

    let previous = find_previous_run(&ctx.results_root, &comparison_key)?;
    let manifest = RunManifest {
        run_id: run_id.clone(),
        timestamp_unix: unix_now()?,
        mode: mode.to_string(),
        client: common.client,
        backend: common.backend,
        ciphers: common.ciphers.clone(),
        sizes_mb: common.sizes.clone(),
        operations: operations.clone(),
        comparison_key: comparison_key.clone(),
        profile_mode,
        matrix_baseline,
        valid_for_comparison: true,
        invalid_reason: None,
        notes: common.note.clone(),
        machine: machine_metadata(),
        targets: plans.iter().map(TargetManifest::from).collect(),
    };

    write_json(run_dir.join("manifest.json"), &manifest)?;

    let bench_client_bin = if common.client == ClientKind::Bench {
        Some(build_bench_client(ctx, common.no_sccache)?)
    } else {
        None
    };

    let mut records = Vec::new();
    for plan in &plans {
        let server_binary = build_server(ctx, plan, common.no_sccache, profile_mode)?;
        let server_target_dir = target_dir(ctx, &plan.build_id);
        println!("== {} ({}) ==", plan.label, plan.source_mode);
        for size_mb in &common.sizes {
            let test_file = ensure_test_file(&ctx.cache_root, *size_mb)?;
            let defaults = default_iterations(*size_mb, profile_mode);
            let warmup = common.warmup.unwrap_or(defaults.0);
            let runs = common.runs.unwrap_or(defaults.1);
            for operation in &operations {
                let artifact_prefix = format!(
                    "{}-{}-{}mb-{}",
                    plan.label,
                    operation_name(*operation),
                    size_mb,
                    client_name(common.client)
                );
                let server_log = run_dir.join(format!("server-{artifact_prefix}.log"));
                let artifact_dir = run_dir.join("artifacts");
                fs::create_dir_all(&artifact_dir)?;
                let port = pick_free_port()?;
                let mut child = start_server(
                    &server_binary,
                    &server_log,
                    &ctx.keys,
                    port,
                    common.backend,
                    profile_mode,
                    perf_frequency.unwrap_or(999),
                    &artifact_dir,
                    &artifact_prefix,
                )?;
                wait_for_server(port, &ctx.keys.private_key, &common.ciphers)?;

                let measured = match common.client {
                    ClientKind::Bench => run_bench_client(
                        bench_client_bin
                            .as_ref()
                            .expect("bench client binary should exist"),
                        &common,
                        *operation,
                        *size_mb,
                        warmup,
                        runs,
                        port,
                        &run_dir,
                        &artifact_prefix,
                    )?,
                    ClientKind::Openssh => run_openssh(
                        &common,
                        *operation,
                        *size_mb,
                        warmup,
                        runs,
                        port,
                        &ctx.keys,
                        &test_file,
                        &run_dir,
                        &artifact_prefix,
                    )?,
                };

                stop_child(&mut child)?;
                records.push(MeasurementRecord::new(
                    plan.label.clone(),
                    *operation,
                    *size_mb,
                    measured.bytes,
                    measured.iterations,
                ));
                let _ = server_target_dir;
            }
        }
    }

    let results = RunResults {
        run_id: run_id.clone(),
        comparison_key,
        records,
    };
    write_json(run_dir.join("results.json"), &results)?;
    let summary = render_summary(&manifest, &results, previous.as_ref());
    fs::write(run_dir.join("summary.txt"), &summary)?;
    print!("{summary}");
    if let Some(previous) = previous {
        println!("Previous comparable run: {}", previous.run_id);
    }
    Ok(())
}

fn profile_label(args: &ProfileArgs) -> String {
    if args.russh_ref.is_some() || args.russh_sftp_ref.is_some() {
        "local-stack".to_string()
    } else {
        "current".to_string()
    }
}

impl TargetManifest {
    fn from(plan: &BuildPlan) -> Self {
        Self {
            label: plan.label.clone(),
            source_mode: plan.source_mode.clone(),
            snapshot: plan.snapshot.clone(),
            russh_ref: plan.russh_ref.clone(),
            russh_sftp_ref: plan.russh_sftp_ref.clone(),
            features: plan.features.clone(),
            manifest_source: plan.manifest_source.display().to_string(),
            build_id: plan.build_id.clone(),
        }
    }
}

#[derive(Debug)]
struct MeasurementSeries {
    bytes: u64,
    iterations: Vec<f64>,
}

impl MeasurementRecord {
    fn new(
        target_label: String,
        operation: OperationKind,
        size_mb: u64,
        bytes_per_iteration: u64,
        iterations_seconds: Vec<f64>,
    ) -> Self {
        let mean_seconds = mean(&iterations_seconds);
        let stddev_seconds = stddev(&iterations_seconds, mean_seconds);
        let min_seconds = iterations_seconds
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min);
        let max_seconds = iterations_seconds
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        let mib = bytes_per_iteration as f64 / (1024.0 * 1024.0);
        let throughput_mib_per_second = mib / mean_seconds;
        Self {
            target_label,
            operation,
            size_mb,
            bytes_per_iteration,
            iterations_seconds,
            mean_seconds,
            stddev_seconds,
            min_seconds,
            max_seconds,
            throughput_mib_per_second,
        }
    }
}

fn make_build_plan(ctx: &AppContext, target: &TargetSpec) -> Result<BuildPlan, BoxError> {
    match &target.source {
        SourceSpec::Current { snapshot } => Ok(BuildPlan {
            label: target.label.clone(),
            manifest_source: ctx.repo_root.clone(),
            build_id: format!(
                "current-{}-{}",
                sanitize_for_path(snapshot),
                sanitize_for_path(&target.label)
            ),
            features: target.features.clone(),
            snapshot: snapshot.clone(),
            russh_ref: None,
            russh_sftp_ref: None,
            source_mode: "current".to_string(),
        }),
        SourceSpec::LocalStack {
            sftp_s3_ref,
            russh_repo,
            russh_ref,
            russh_sftp_repo,
            russh_sftp_ref,
        } => {
            let sftp_snapshot = if sftp_s3_ref == "HEAD" {
                snapshot_id(&ctx.repo_root)?
            } else {
                resolve_commit(&ctx.repo_root, sftp_s3_ref)?
            };
            let russh_commit = resolve_commit(russh_repo, russh_ref)?;
            let sftp_commit = resolve_commit(russh_sftp_repo, russh_sftp_ref)?;
            let build_id = format!(
                "{}-{}-{}-{}",
                sanitize_for_path(&target.label),
                short_commit(&sftp_snapshot),
                short_commit(&russh_commit),
                short_commit(&sftp_commit)
            );
            let manifest_source = prepare_local_stack_source(
                ctx,
                &build_id,
                &sftp_snapshot,
                russh_repo,
                &russh_commit,
                russh_sftp_repo,
                &sftp_commit,
                russh_sftp_ref == "master",
            )?;
            Ok(BuildPlan {
                label: target.label.clone(),
                manifest_source,
                build_id,
                features: target.features.clone(),
                snapshot: sftp_snapshot,
                russh_ref: Some(russh_ref.clone()),
                russh_sftp_ref: Some(russh_sftp_ref.clone()),
                source_mode: "local-stack".to_string(),
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_local_stack_source(
    ctx: &AppContext,
    build_id: &str,
    sftp_snapshot: &str,
    russh_repo: &Path,
    russh_commit: &str,
    russh_sftp_repo: &Path,
    russh_sftp_commit: &str,
    sftp_master_compat: bool,
) -> Result<PathBuf, BoxError> {
    let sftp_dir = worktree_dir(&ctx.cache_root, "sftp-s3", build_id);
    let russh_dir = worktree_dir(&ctx.cache_root, "russh", russh_commit);
    let russh_sftp_dir = worktree_dir(&ctx.cache_root, "russh-sftp", russh_sftp_commit);
    ensure_worktree(&ctx.repo_root, sftp_snapshot, &sftp_dir)?;
    ensure_worktree(russh_repo, russh_commit, &russh_dir)?;
    ensure_worktree(russh_sftp_repo, russh_sftp_commit, &russh_sftp_dir)?;
    rewrite_dependency_paths(
        &sftp_dir.join("Cargo.toml"),
        &russh_dir.join("russh"),
        &russh_sftp_dir,
    )?;
    if sftp_master_compat {
        rewrite_sftp_master_compat(&sftp_dir.join("src/sftp_handler.rs"))?;
    }
    let _ = fs::remove_file(sftp_dir.join("Cargo.lock"));
    Ok(sftp_dir)
}

fn rewrite_sftp_master_compat(path: &Path) -> Result<(), BoxError> {
    const CURRENT_BLOCK: &str = r#"type SftpHandle = Bytes;

type SftpWriteData = Bytes;

fn handle_as_bytes(h: &SftpHandle) -> &[u8] {
    h.as_ref()
}

struct HandleForLog<'a>(&'a SftpHandle);

impl fmt::Display for HandleForLog<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in handle_as_bytes(self.0) {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

fn write_data_into_bytes(data: SftpWriteData) -> Bytes {
    data
}

fn sftp_data(id: u32, data: Bytes) -> Data {
    Data { id, data }
}"#;

    const MASTER_BLOCK: &str = r#"type SftpHandle = String;

type SftpWriteData = Vec<u8>;

fn handle_as_bytes(h: &SftpHandle) -> &[u8] {
    h.as_bytes()
}

struct HandleForLog<'a>(&'a SftpHandle);

impl fmt::Display for HandleForLog<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

fn write_data_into_bytes(data: SftpWriteData) -> Bytes {
    Bytes::from(data)
}

fn sftp_data(id: u32, data: Bytes) -> Data {
    Data {
        id,
        data: data.to_vec(),
    }
}"#;

    let content = fs::read_to_string(path)?;
    let rewritten = content.replace(CURRENT_BLOCK, MASTER_BLOCK);
    if rewritten == content {
        return Err(format!(
            "failed to apply sftp-master compatibility rewrite to {}",
            path.display()
        )
        .into());
    }
    fs::write(path, rewritten)?;
    Ok(())
}

fn build_bench_client(ctx: &AppContext, no_sccache: bool) -> Result<PathBuf, BoxError> {
    let target_dir = ctx.cache_root.join("targets").join("bench-client");
    fs::create_dir_all(&target_dir)?;
    let mut command = Command::new("cargo");
    command
        .current_dir(&ctx.repo_root)
        .arg("build")
        .arg("--release")
        .arg("--bin")
        .arg("sftp-bench-client");
    command.env("CARGO_TARGET_DIR", &target_dir);
    if !no_sccache {
        if let Some(bin) = &ctx.sccache_bin {
            command.env("RUSTC_WRAPPER", bin);
        }
    }
    run_status(&mut command, "build sftp-bench-client")?;
    Ok(target_dir.join("release").join("sftp-bench-client"))
}

fn build_server(
    ctx: &AppContext,
    plan: &BuildPlan,
    no_sccache: bool,
    profile_mode: Option<ProfileKind>,
) -> Result<PathBuf, BoxError> {
    let target_dir = target_dir(ctx, &plan.build_id);
    fs::create_dir_all(&target_dir)?;
    let mut command = Command::new("cargo");
    command.current_dir(&plan.manifest_source);
    command.arg("build").arg("--bin").arg("sftp-s3");
    match profile_mode {
        Some(_) => {
            command.arg("--profile").arg("profiling");
        }
        None => {
            command.arg("--release");
        }
    }
    if !plan.features.is_empty() {
        command.arg("--features").arg(plan.features.join(","));
    }
    command.env("CARGO_TARGET_DIR", &target_dir);
    command.env("RUSTFLAGS", "-C target-cpu=native");
    if !no_sccache {
        if let Some(bin) = &ctx.sccache_bin {
            command.env("RUSTC_WRAPPER", bin);
        }
    }
    run_status(&mut command, &format!("build {}", plan.label))?;
    Ok(match profile_mode {
        Some(_) => target_dir.join("profiling").join("sftp-s3"),
        None => target_dir.join("release").join("sftp-s3"),
    })
}

#[allow(clippy::too_many_arguments)]
fn run_bench_client(
    binary: &Path,
    common: &CommonArgs,
    operation: OperationKind,
    size_mb: u64,
    warmup: u32,
    runs: u32,
    port: u16,
    run_dir: &Path,
    artifact_prefix: &str,
) -> Result<MeasurementSeries, BoxError> {
    if warmup > 0 {
        let warmup_path = run_dir
            .join("artifacts")
            .join(format!("{artifact_prefix}-warmup.json"));
        invoke_bench_client(
            binary,
            common,
            operation,
            size_mb,
            warmup,
            port,
            &warmup_path,
        )?;
    }
    let output_path = run_dir
        .join("artifacts")
        .join(format!("{artifact_prefix}.json"));
    let output = invoke_bench_client(binary, common, operation, size_mb, runs, port, &output_path)?;
    let iterations = output
        .results
        .iter()
        .map(|result| match operation {
            OperationKind::Upload => result.upload_seconds.unwrap_or(result.total_seconds),
            OperationKind::Download => result.download_seconds.unwrap_or(result.total_seconds),
            OperationKind::Roundtrip => result.total_seconds,
        })
        .collect();
    Ok(MeasurementSeries {
        bytes: bytes_for_operation(operation, size_mb),
        iterations,
    })
}

fn invoke_bench_client(
    binary: &Path,
    common: &CommonArgs,
    operation: OperationKind,
    size_mb: u64,
    iterations: u32,
    port: u16,
    output_path: &Path,
) -> Result<BenchClientJson, BoxError> {
    let mut command = Command::new(binary);
    command
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--user")
        .arg("benchmark")
        .arg("--password")
        .arg("benchmark")
        .arg("--operation")
        .arg(operation_name(operation))
        .arg("--size")
        .arg(format!("{size_mb}MiB"))
        .arg("--iterations")
        .arg(iterations.to_string())
        .arg("--chunk-size")
        .arg(&common.chunk_size)
        .arg("--insecure")
        .arg("--json-output")
        .arg(output_path);
    if let Some(ciphers) = &common.ciphers {
        command.arg("--ciphers").arg(ciphers);
    }
    run_status(&mut command, "run sftp-bench-client")?;
    read_json(output_path)
}

#[allow(clippy::too_many_arguments)]
fn run_openssh(
    common: &CommonArgs,
    operation: OperationKind,
    size_mb: u64,
    warmup: u32,
    runs: u32,
    port: u16,
    keys: &KeyMaterial,
    test_file: &Path,
    run_dir: &Path,
    artifact_prefix: &str,
) -> Result<MeasurementSeries, BoxError> {
    for iteration in 0..warmup {
        let _ = run_openssh_iteration(
            common,
            operation,
            size_mb,
            port,
            keys,
            test_file,
            run_dir,
            &format!("{artifact_prefix}-warmup-{iteration}"),
        )?;
    }
    let mut iterations = Vec::new();
    for iteration in 0..runs {
        iterations.push(run_openssh_iteration(
            common,
            operation,
            size_mb,
            port,
            keys,
            test_file,
            run_dir,
            &format!("{artifact_prefix}-{iteration}"),
        )?);
    }
    Ok(MeasurementSeries {
        bytes: bytes_for_operation(operation, size_mb),
        iterations,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_openssh_iteration(
    common: &CommonArgs,
    operation: OperationKind,
    size_mb: u64,
    port: u16,
    keys: &KeyMaterial,
    test_file: &Path,
    run_dir: &Path,
    artifact_prefix: &str,
) -> Result<f64, BoxError> {
    let remote_file = format!("bench-{}-{}.bin", artifact_prefix, size_mb);
    let download_file = run_dir
        .join("artifacts")
        .join(format!("download-{artifact_prefix}.bin"));
    match operation {
        OperationKind::Upload => {
            let start = Instant::now();
            run_sftp_batch(
                port,
                keys,
                common.ciphers.as_deref(),
                &[
                    format!("put {} {}", test_file.display(), remote_file),
                    "quit".to_string(),
                ],
            )?;
            let elapsed = start.elapsed().as_secs_f64();
            let _ = run_sftp_batch(
                port,
                keys,
                common.ciphers.as_deref(),
                &[format!("rm {}", remote_file), "quit".to_string()],
            );
            Ok(elapsed)
        }
        OperationKind::Download => {
            run_sftp_batch(
                port,
                keys,
                common.ciphers.as_deref(),
                &[
                    format!("put {} {}", test_file.display(), remote_file),
                    "quit".to_string(),
                ],
            )?;
            let start = Instant::now();
            run_sftp_batch(
                port,
                keys,
                common.ciphers.as_deref(),
                &[
                    format!("get {} {}", remote_file, download_file.display()),
                    "quit".to_string(),
                ],
            )?;
            let elapsed = start.elapsed().as_secs_f64();
            let _ = run_sftp_batch(
                port,
                keys,
                common.ciphers.as_deref(),
                &[format!("rm {}", remote_file), "quit".to_string()],
            );
            let _ = fs::remove_file(download_file);
            Ok(elapsed)
        }
        OperationKind::Roundtrip => {
            let start = Instant::now();
            run_sftp_batch(
                port,
                keys,
                common.ciphers.as_deref(),
                &[
                    format!("put {} {}", test_file.display(), remote_file),
                    "quit".to_string(),
                ],
            )?;
            run_sftp_batch(
                port,
                keys,
                common.ciphers.as_deref(),
                &[
                    format!("get {} {}", remote_file, download_file.display()),
                    "quit".to_string(),
                ],
            )?;
            let elapsed = start.elapsed().as_secs_f64();
            let _ = run_sftp_batch(
                port,
                keys,
                common.ciphers.as_deref(),
                &[format!("rm {}", remote_file), "quit".to_string()],
            );
            let _ = fs::remove_file(download_file);
            Ok(elapsed)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_server(
    binary: &Path,
    log_path: &Path,
    keys: &KeyMaterial,
    port: u16,
    backend: BackendKind,
    profile_mode: Option<ProfileKind>,
    perf_frequency: u32,
    artifact_dir: &Path,
    artifact_prefix: &str,
) -> Result<Child, BoxError> {
    let log = fs::File::create(log_path)?;
    let log_err = log.try_clone()?;
    let backend_name = match backend {
        BackendKind::Benchmark => "benchmark",
        BackendKind::Memory => "memory",
    };
    let args = vec![
        "--port".to_string(),
        port.to_string(),
        "--authorized-keys-file".to_string(),
        keys.authorized_keys.display().to_string(),
        "--user".to_string(),
        "benchmark:benchmark".to_string(),
        "--backend".to_string(),
        backend_name.to_string(),
    ];
    let mut command = match profile_mode {
        Some(ProfileKind::Perf) => {
            let mut command = Command::new("perf");
            command
                .arg("record")
                .arg("-F")
                .arg(perf_frequency.to_string())
                .arg("-g")
                .arg("-o")
                .arg(artifact_dir.join(format!("{artifact_prefix}.perf.data")));
            command.arg("--").arg(binary);
            command.args(&args);
            command
        }
        Some(ProfileKind::Heaptrack) => {
            let mut command = Command::new("heaptrack");
            command
                .arg("-o")
                .arg(artifact_dir.join(format!("{artifact_prefix}.heaptrack")));
            command.arg(binary);
            command.args(&args);
            command
        }
        None => {
            let mut command = Command::new(binary);
            command.args(&args);
            command
        }
    };
    command.stdout(Stdio::from(log));
    command.stderr(Stdio::from(log_err));
    Ok(command.spawn()?)
}

fn wait_for_server(
    port: u16,
    identity_file: &Path,
    ciphers: &Option<String>,
) -> Result<(), BoxError> {
    for _ in 0..50 {
        let mut command = Command::new("sftp");
        command
            .arg("-q")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(format!("IdentityFile={}", identity_file.display()))
            .arg("-o")
            .arg("StrictHostKeyChecking=no")
            .arg("-o")
            .arg("UserKnownHostsFile=/dev/null")
            .arg("-o")
            .arg("Compression=no")
            .arg("-b")
            .arg("-")
            .arg("-P")
            .arg(port.to_string())
            .arg("benchmark@127.0.0.1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(cipher_list) = ciphers {
            command.arg("-c").arg(openssh_ciphers(cipher_list));
        }
        let mut child = command.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(b"bye\n")?;
        }
        if child.wait()?.success() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err("server did not become ready".into())
}

fn stop_child(child: &mut Child) -> Result<(), BoxError> {
    if child.try_wait()?.is_none() {
        child.kill()?;
    }
    let status = child.wait()?;
    #[cfg(unix)]
    if status.signal() == Some(9) {
        return Ok(());
    }
    if status.success() || status.code() == Some(130) || status.code() == Some(143) {
        Ok(())
    } else {
        Err(format!("server exited unsuccessfully: {status}").into())
    }
}

fn run_sftp_batch(
    port: u16,
    keys: &KeyMaterial,
    ciphers: Option<&str>,
    commands: &[String],
) -> Result<(), BoxError> {
    let mut command = Command::new("sftp");
    command
        .arg("-q")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg(format!("IdentityFile={}", keys.private_key.display()))
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null")
        .arg("-o")
        .arg("Compression=no")
        .arg("-b")
        .arg("-")
        .arg("-P")
        .arg(port.to_string())
        .arg("benchmark@127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cipher_list) = ciphers {
        command.arg("-c").arg(openssh_ciphers(cipher_list));
    }
    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        for line in commands {
            stdin.write_all(line.as_bytes())?;
            stdin.write_all(b"\n")?;
        }
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("sftp batch failed with {status}").into())
    }
}

fn render_summary(
    manifest: &RunManifest,
    results: &RunResults,
    previous: Option<&PreviousRun>,
) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "Run {} ({}) client={} backend={}\n",
        manifest.run_id,
        manifest.mode,
        client_name(manifest.client),
        backend_name(manifest.backend)
    ));
    output.push_str(&format!(
        "Artifacts: benchmark_results/runs/{}/artifacts\n",
        manifest.run_id
    ));
    output.push_str(&format!(
        "Machine: {} {} cpu={} host={}\n",
        manifest.machine.os, manifest.machine.arch, manifest.machine.cpu, manifest.machine.hostname
    ));
    output.push_str(&format!(
        "Comparison status: {}\n",
        if manifest.valid_for_comparison {
            "valid"
        } else {
            "invalid"
        }
    ));
    if let Some(reason) = &manifest.invalid_reason {
        output.push_str(&format!("Invalid reason: {reason}\n"));
    }
    for note in &manifest.notes {
        output.push_str(&format!("Note: {note}\n"));
    }
    if let Some(ciphers) = &manifest.ciphers {
        output.push_str(&format!("Ciphers: {ciphers}\n"));
    }

    let mut grouped: BTreeMap<(&str, u64), BTreeMap<OperationKind, &MeasurementRecord>> =
        BTreeMap::new();
    for record in &results.records {
        grouped
            .entry((&record.target_label, record.size_mb))
            .or_default()
            .insert(record.operation, record);
    }

    for ((label, size_mb), operations) in grouped {
        output.push_str(&format!("\n{label} {size_mb}MiB\n"));
        for operation in [
            OperationKind::Upload,
            OperationKind::Download,
            OperationKind::Roundtrip,
        ] {
            if let Some(record) = operations.get(&operation) {
                output.push_str(&format!(
                    "  {:>9}: {:8.1} MiB/s  mean {:6.3}s  stddev {:6.3}s\n",
                    operation_name(operation),
                    record.throughput_mib_per_second,
                    record.mean_seconds,
                    record.stddev_seconds
                ));
                if let Some(previous_run) = previous {
                    if let Some(previous_record) =
                        previous_run.results.records.iter().find(|candidate| {
                            candidate.target_label == record.target_label
                                && candidate.size_mb == record.size_mb
                                && candidate.operation == record.operation
                        })
                    {
                        let delta = ((record.throughput_mib_per_second
                            / previous_record.throughput_mib_per_second)
                            - 1.0)
                            * 100.0;
                        output.push_str(&format!(
                            "             vs {:>10}: {:+6.1}%\n",
                            previous_run.run_id, delta
                        ));
                    }
                }
            }
        }
    }

    if let Some(baseline_label) = manifest.matrix_baseline.as_deref() {
        output.push_str(&format!("\nMatrix deltas vs {baseline_label}\n"));
        for operation in [
            OperationKind::Upload,
            OperationKind::Download,
            OperationKind::Roundtrip,
        ] {
            for size_mb in &manifest.sizes_mb {
                let baseline = results.records.iter().find(|record| {
                    record.target_label == baseline_label
                        && record.size_mb == *size_mb
                        && record.operation == operation
                });
                let Some(baseline) = baseline else {
                    continue;
                };
                output.push_str(&format!(
                    "  {} {}MiB baseline {:8.1} MiB/s\n",
                    operation_name(operation),
                    size_mb,
                    baseline.throughput_mib_per_second
                ));
                for record in results.records.iter().filter(|record| {
                    record.target_label != baseline_label
                        && record.size_mb == *size_mb
                        && record.operation == operation
                }) {
                    let delta = ((record.throughput_mib_per_second
                        / baseline.throughput_mib_per_second)
                        - 1.0)
                        * 100.0;
                    output.push_str(&format!(
                        "    {:>16}: {:8.1} MiB/s ({:+6.1}%)\n",
                        record.target_label, record.throughput_mib_per_second, delta
                    ));
                }
            }
        }
    }

    output
}

fn find_previous_run(
    results_root: &Path,
    comparison_key: &str,
) -> Result<Option<PreviousRun>, BoxError> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(results_root)? {
        let entry = entry?;
        let manifest_path = entry.path().join("manifest.json");
        let results_path = entry.path().join("results.json");
        if !manifest_path.exists() || !results_path.exists() {
            continue;
        }
        let manifest: RunManifest = read_json(&manifest_path)?;
        if manifest.comparison_key == comparison_key && manifest.valid_for_comparison {
            candidates.push((
                manifest.timestamp_unix,
                manifest.run_id.clone(),
                manifest,
                results_path,
            ));
        }
    }
    candidates.sort_by_key(|candidate| candidate.0);
    if let Some((_, run_id, manifest, results_path)) = candidates.pop() {
        let results = read_json(&results_path)?;
        let _ = manifest;
        Ok(Some(PreviousRun { run_id, results }))
    } else {
        Ok(None)
    }
}

fn list_runs(ctx: &AppContext, args: ListArgs) -> Result<(), BoxError> {
    let mut runs = load_runs(&ctx.results_root)?;
    runs.sort_by_key(|run| run.manifest.timestamp_unix);
    runs.reverse();

    for run in runs
        .into_iter()
        .filter(|run| args.all || run.manifest.valid_for_comparison)
        .take(args.limit)
    {
        let validity = if run.manifest.valid_for_comparison {
            "valid"
        } else {
            "invalid"
        };
        let note = run
            .manifest
            .invalid_reason
            .clone()
            .or_else(|| run.manifest.notes.first().cloned())
            .unwrap_or_default();
        println!(
            "{}  {:10}  {:9}  {:8}  {}",
            run.manifest.run_id,
            run.manifest.mode,
            client_name(run.manifest.client),
            validity,
            note
        );
    }
    Ok(())
}

fn show_run(ctx: &AppContext, run_id: &str) -> Result<(), BoxError> {
    let run_dir = ctx.results_root.join(run_id);
    let manifest: RunManifest = read_json(&run_dir.join("manifest.json"))?;
    let results: RunResults = read_json(&run_dir.join("results.json"))?;
    let summary = render_summary(&manifest, &results, None);
    print!("{summary}");
    Ok(())
}

fn mark_run_invalid(ctx: &AppContext, run_id: &str, reason: &str) -> Result<(), BoxError> {
    update_run_validity(ctx, run_id, false, Some(reason.to_string()))
}

fn mark_run_valid(ctx: &AppContext, run_id: &str) -> Result<(), BoxError> {
    update_run_validity(ctx, run_id, true, None)
}

fn update_run_validity(
    ctx: &AppContext,
    run_id: &str,
    valid_for_comparison: bool,
    invalid_reason: Option<String>,
) -> Result<(), BoxError> {
    let run_dir = ctx.results_root.join(run_id);
    let manifest_path = run_dir.join("manifest.json");
    let results_path = run_dir.join("results.json");
    let summary_path = run_dir.join("summary.txt");
    let mut manifest: RunManifest = read_json(&manifest_path)?;

    manifest.valid_for_comparison = valid_for_comparison;
    manifest.invalid_reason = invalid_reason;
    write_json(manifest_path, &manifest)?;
    if results_path.exists() {
        let results: RunResults = read_json(&results_path)?;
        fs::write(summary_path, render_summary(&manifest, &results, None))?;
    }
    println!(
        "{} marked {}",
        run_id,
        if valid_for_comparison {
            "valid"
        } else {
            "invalid"
        }
    );
    Ok(())
}

fn load_runs(results_root: &Path) -> Result<Vec<RunRecord>, BoxError> {
    let mut runs = Vec::new();
    for entry in fs::read_dir(results_root)? {
        let entry = entry?;
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }
        let manifest: RunManifest = read_json(&manifest_path)?;
        runs.push(RunRecord { manifest });
    }
    Ok(runs)
}

fn target_dir(ctx: &AppContext, build_id: &str) -> PathBuf {
    ctx.cache_root.join("targets").join(build_id)
}

fn machine_metadata() -> MachineMetadata {
    MachineMetadata {
        os: os_name(),
        arch: std::env::consts::ARCH.to_string(),
        hostname: hostname().unwrap_or_else(|| "unknown".to_string()),
        cpu: cpu_name(),
    }
}

fn os_name() -> String {
    linux_pretty_name()
        .or_else(uname_os_name)
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}

fn hostname() -> Option<String> {
    let output = Command::new("hostname").output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn cpu_name() -> String {
    linux_cpu_name()
        .or_else(sysctl_cpu_name)
        .or_else(lscpu_cpu_name)
        .unwrap_or_else(unknown_string)
}

fn linux_pretty_name() -> Option<String> {
    let contents = fs::read_to_string("/etc/os-release").ok()?;
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
            return Some(value.trim_matches('"').to_string());
        }
    }
    None
}

fn uname_os_name() -> Option<String> {
    let output = Command::new("uname").args(["-sr"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn linux_cpu_name() -> Option<String> {
    let contents = fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("model name") {
            let (_, value) = value.split_once(':')?;
            return Some(value.trim().to_string());
        }
    }
    None
}

fn sysctl_cpu_name() -> Option<String> {
    let output = Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn lscpu_cpu_name() -> Option<String> {
    let output = Command::new("lscpu").output().ok()?;
    if !output.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(value) = line.strip_prefix("Model name:") {
            let name = value.trim().to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

fn write_json<T: Serialize>(path: PathBuf, value: &T) -> Result<(), BoxError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(path, bytes)?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, BoxError> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn ensure_key_material(cache_root: &Path) -> Result<KeyMaterial, BoxError> {
    let key_dir = cache_root.join("keys");
    fs::create_dir_all(&key_dir)?;
    let private_key = key_dir.join("id_ed25519");
    let authorized_keys = key_dir.join("id_ed25519.pub");
    if !private_key.exists() || !authorized_keys.exists() {
        run_status(
            Command::new("ssh-keygen")
                .arg("-q")
                .arg("-t")
                .arg("ed25519")
                .arg("-N")
                .arg("")
                .arg("-f")
                .arg(&private_key),
            "generate benchmark ssh key",
        )?;
    }
    Ok(KeyMaterial {
        private_key,
        authorized_keys,
    })
}

fn ensure_test_file(cache_root: &Path, size_mb: u64) -> Result<PathBuf, BoxError> {
    let dir = cache_root.join("testfiles");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("testfile_{size_mb}mb.bin"));
    if !path.exists() {
        let file = fs::File::create(&path)?;
        file.set_len(size_mb * 1024 * 1024)?;
    }
    Ok(path)
}

fn ensure_sizes(sizes: &[u64]) -> Result<(), BoxError> {
    if sizes.is_empty() {
        return Err("at least one --sizes value is required".into());
    }
    if sizes.contains(&0) {
        return Err("size values must be greater than zero".into());
    }
    Ok(())
}

fn selected_operations(selection: OperationSelection) -> Vec<OperationKind> {
    match selection {
        OperationSelection::Upload => vec![OperationKind::Upload],
        OperationSelection::Download => vec![OperationKind::Download],
        OperationSelection::Roundtrip => vec![OperationKind::Roundtrip],
        OperationSelection::All => vec![
            OperationKind::Upload,
            OperationKind::Download,
            OperationKind::Roundtrip,
        ],
    }
}

fn default_iterations(size_mb: u64, profile_mode: Option<ProfileKind>) -> (u32, u32) {
    if profile_mode.is_some() {
        return (0, 1);
    }
    match size_mb {
        0..=32 => (1, 5),
        33..=256 => (1, 3),
        257..=1024 => (1, 5),
        _ => (0, 2),
    }
}

fn bytes_for_operation(operation: OperationKind, size_mb: u64) -> u64 {
    let bytes = size_mb * 1024 * 1024;
    match operation {
        OperationKind::Upload | OperationKind::Download => bytes,
        OperationKind::Roundtrip => bytes * 2,
    }
}

fn matrix_label(reference: &str) -> &'static str {
    match reference {
        "main" | "master" => "current",
        "mjc/own-inbound-channel-payloads" | "deserialize-bytes-optimization" => "mjc",
        _ => "custom",
    }
}

fn worktree_dir(cache_root: &Path, component: &str, key: &str) -> PathBuf {
    cache_root
        .join("worktrees")
        .join(component)
        .join(sanitize_for_path(key))
}

fn ensure_worktree(repo: &Path, reference: &str, path: &Path) -> Result<(), BoxError> {
    if path.exists() {
        let current = resolve_commit(path, "HEAD")?;
        let desired = resolve_commit(repo, reference)?;
        if current == desired {
            return Ok(());
        }
        run_status(
            Command::new("git")
                .arg("-C")
                .arg(repo)
                .arg("worktree")
                .arg("remove")
                .arg("--force")
                .arg(path),
            &format!("remove worktree {}", path.display()),
        )?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    run_status(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("worktree")
            .arg("add")
            .arg("--detach")
            .arg(path)
            .arg(reference),
        &format!("create worktree {}", path.display()),
    )
}

fn rewrite_dependency_paths(
    cargo_toml: &Path,
    russh_path: &Path,
    russh_sftp_path: &Path,
) -> Result<(), BoxError> {
    let original = fs::read_to_string(cargo_toml)?;
    let rewritten = original
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("russh = ") {
                format!(
                    "russh = {{ path = \"{}\", default-features = false, features = [\"aws-lc-rs\", \"flate2\"] }}",
                    russh_path.display()
                )
            } else if trimmed.starts_with("russh-sftp = ") {
                format!("russh-sftp = {{ path = \"{}\" }}", russh_sftp_path.display())
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(cargo_toml, rewritten)?;
    Ok(())
}

fn build_run_id(mode: &str, label: Option<&str>) -> String {
    let ts = unix_now().unwrap_or_default();
    match label {
        Some(label) => format!(
            "{ts}-{}-{}",
            sanitize_for_path(mode),
            sanitize_for_path(label)
        ),
        None => format!("{ts}-{}", sanitize_for_path(mode)),
    }
}

#[allow(clippy::too_many_arguments)]
fn make_comparison_key(
    mode: &str,
    client: ClientKind,
    backend: BackendKind,
    ciphers: Option<&str>,
    sizes: &[u64],
    operations: &[OperationKind],
    targets: &[BuildPlan],
    profile_mode: Option<ProfileKind>,
) -> String {
    let target_labels = targets
        .iter()
        .map(|target| target.label.clone())
        .collect::<Vec<_>>()
        .join(",");
    let sizes = sizes
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let operations = operations
        .iter()
        .map(|operation| operation_name(*operation).to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "mode={mode};client={};backend={};ciphers={};sizes={sizes};ops={operations};targets={target_labels};profile={}",
        client_name(client),
        backend_name(backend),
        ciphers.unwrap_or("default"),
        profile_mode
            .map(|mode| match mode {
                ProfileKind::Perf => "perf",
                ProfileKind::Heaptrack => "heaptrack",
            })
            .unwrap_or("none"),
    )
}

fn repo_root() -> Result<PathBuf, BoxError> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()?;
    if !output.status.success() {
        return Err("failed to locate repository root".into());
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

fn snapshot_id(repo: &Path) -> Result<String, BoxError> {
    if git_is_dirty(repo)? {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("stash")
            .arg("create")
            .arg("sftp-perf-snapshot")
            .output()?;
        let snapshot = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !snapshot.is_empty() {
            return Ok(snapshot);
        }
    }
    resolve_commit(repo, "HEAD")
}

fn git_is_dirty(repo: &Path) -> Result<bool, BoxError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("status")
        .arg("--porcelain")
        .arg("--untracked-files=no")
        .output()?;
    if !output.status.success() {
        return Err(format!("failed to inspect git status in {}", repo.display()).into());
    }
    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

fn resolve_commit(repo: &Path, reference: &str) -> Result<String, BoxError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg(format!("{reference}^{{commit}}"))
        .output()?;
    if !output.status.success() {
        return Err(format!("failed to resolve {reference} in {}", repo.display()).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn pick_free_port() -> Result<u16, BoxError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn run_status(command: &mut Command, description: &str) -> Result<(), BoxError> {
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{description} failed with {status}").into())
    }
}

fn command_path(name: &str) -> Option<PathBuf> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

fn sanitize_for_path(input: &str) -> String {
    input
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => ch,
            _ => '-',
        })
        .collect()
}

fn short_commit(commit: &str) -> &str {
    commit.get(..8).unwrap_or(commit)
}

fn openssh_ciphers(raw: &str) -> String {
    raw.split(',')
        .filter(|part| !part.trim().is_empty())
        .map(|cipher| match cipher.trim() {
            "aes256-gcm" => "aes256-gcm@openssh.com".to_string(),
            "chacha20-poly1305" => "chacha20-poly1305@openssh.com".to_string(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn operation_name(operation: OperationKind) -> &'static str {
    match operation {
        OperationKind::Upload => "upload",
        OperationKind::Download => "download",
        OperationKind::Roundtrip => "roundtrip",
    }
}

fn client_name(client: ClientKind) -> &'static str {
    match client {
        ClientKind::Bench => "bench",
        ClientKind::Openssh => "openssh",
    }
}

fn backend_name(backend: BackendKind) -> &'static str {
    match backend {
        BackendKind::Benchmark => "benchmark",
        BackendKind::Memory => "memory",
    }
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

fn stddev(values: &[f64], mean: f64) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let variance = values
        .iter()
        .map(|value| {
            let delta = value - mean;
            delta * delta
        })
        .sum::<f64>()
        / values.len() as f64;
    variance.sqrt()
}

fn unix_now() -> Result<u64, BoxError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
