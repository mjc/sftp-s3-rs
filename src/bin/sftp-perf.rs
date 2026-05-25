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
    /// Benchmark a flat directory containing many small files
    SmallFiles(SmallFilesArgs),
    /// Benchmark a local stack using specific russh/russh-sftp refs
    LocalStack(LocalStackArgs),
    /// Benchmark a russh × russh-sftp version matrix
    Matrix(MatrixArgs),
    /// Record a profiling trace for the selected workload
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

    /// OpenSSH sftp outstanding request count (-R)
    #[arg(long)]
    sftp_requests: Option<u32>,

    /// OpenSSH sftp transfer buffer size in bytes (-B)
    #[arg(long)]
    sftp_buffer_size: Option<usize>,

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
struct SmallFilesArgs {
    /// Total payload size, in MiB, across all files
    #[arg(long, default_value_t = 1024)]
    total_size_mb: u64,

    /// Number of files in the flat benchmark directory
    #[arg(long, default_value_t = 10_251)]
    files: usize,

    /// Number of measured runs
    #[arg(long)]
    runs: Option<u32>,

    /// Number of warmup runs
    #[arg(long)]
    warmup: Option<u32>,

    /// Preferred cipher list
    #[arg(long, default_value = "aes256-gcm")]
    ciphers: Option<String>,

    /// Client implementation to drive the benchmark
    #[arg(long, value_enum, default_value_t = ClientKind::Bench)]
    client: ClientKind,

    /// Benchmark backend to use
    #[arg(long, value_enum, default_value_t = BackendKind::Benchmark)]
    backend: BackendKind,

    /// Optional label suffix for the run directory
    #[arg(long)]
    label: Option<String>,

    /// Add a note to the stored run metadata
    #[arg(long)]
    note: Vec<String>,

    /// Record a server-side profiling trace while the workload runs
    #[arg(long)]
    profile: bool,

    /// OpenSSH sftp outstanding request count (-R)
    #[arg(long)]
    sftp_requests: Option<u32>,

    /// OpenSSH sftp transfer buffer size in bytes (-B)
    #[arg(long)]
    sftp_buffer_size: Option<usize>,

    /// Chunk size passed to the Rust benchmark client
    #[arg(long, default_value = "64KiB")]
    chunk_size: String,

    /// Number of files the Rust benchmark client processes concurrently
    #[arg(long, default_value_t = 1)]
    file_depth: usize,

    /// Disable sccache even if present
    #[arg(long)]
    no_sccache: bool,
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

    /// Sampling frequency for Linux `perf profile` runs.
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
    #[serde(default)]
    profile_tool: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_count_per_iteration: Option<u64>,
    iterations_seconds: Vec<f64>,
    mean_seconds: f64,
    stddev_seconds: f64,
    min_seconds: f64,
    max_seconds: f64,
    throughput_mib_per_second: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_ops_per_second: Option<f64>,
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
        PerfCommand::SmallFiles(args) => run_small_files(&ctx, args),
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

fn kill_pid(pid: u32) -> Result<(), BoxError> {
    #[cfg(unix)]
    {
        run_status(
            Command::new("kill").arg("-KILL").arg(pid.to_string()),
            &format!("kill process {pid}"),
        )?;
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        return Err("killing launched profile targets is only supported on Unix".into());
    }
    Ok(())
}

fn interrupt_pid(pid: u32) -> Result<(), BoxError> {
    #[cfg(unix)]
    {
        run_status(
            Command::new("kill").arg("-INT").arg(pid.to_string()),
            &format!("interrupt process {pid}"),
        )?;
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        return Err("interrupting launched profile targets is only supported on Unix".into());
    }
    Ok(())
}

fn wait_for_exit(
    child: &mut Child,
    timeout: Duration,
) -> Result<std::process::ExitStatus, BoxError> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if start.elapsed() >= timeout {
            child.kill()?;
            return Ok(child.wait()?);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn export_xctrace(trace_path: &Path, export_path: &Path) -> Result<(), BoxError> {
    wait_for_path(trace_path, Duration::from_secs(10))?;
    let mut last_error = None;
    for _ in 0..5 {
        match run_status(
            Command::new("xctrace")
                .arg("export")
                .arg("--input")
                .arg(trace_path)
                .arg("--xpath")
                .arg("//trace-toc/run[1]/data/table[@schema=\"cpu-profile\"]")
                .arg("--output")
                .arg(export_path),
            &format!("export xctrace {}", trace_path.display()),
        ) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_error = Some(err);
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| "xctrace export failed".into()))
}

fn wait_for_path(path: &Path, timeout: Duration) -> Result<(), BoxError> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("path did not appear in time: {}", path.display()).into())
}

fn read_pid(path: &Path) -> Result<u32, BoxError> {
    let raw = fs::read_to_string(path)?;
    Ok(raw.trim().parse()?)
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
    ensure_common_args(&common)?;
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
        common.sftp_requests,
        common.sftp_buffer_size,
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
        profile_tool: profile_mode.map(profile_tool_name),
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

                stop_server(&mut child)?;
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

fn run_small_files(ctx: &AppContext, args: SmallFilesArgs) -> Result<(), BoxError> {
    ensure_small_files_args(&args)?;
    let profile_mode = args.profile.then_some(ProfileKind::Perf);
    let mode = if args.profile {
        "small-files-profile"
    } else {
        "small-files"
    };
    let run_id = build_run_id(mode, args.label.as_deref());
    let run_dir = ctx.results_root.join(&run_id);
    let artifact_dir = run_dir.join("artifacts");
    fs::create_dir_all(&artifact_dir)?;

    let target = TargetSpec {
        label: "current".to_string(),
        source: SourceSpec::Current {
            snapshot: snapshot_id(&ctx.repo_root)?,
        },
        features: Vec::new(),
    };
    let plan = make_build_plan(ctx, &target)?;
    let total_bytes = mib_to_bytes(args.total_size_mb)?;
    let file_paths = if args.client == ClientKind::Openssh {
        ensure_small_files_fixture(&ctx.cache_root, args.total_size_mb, args.files)?
    } else {
        Vec::new()
    };
    let comparison_key = format!(
        "{};files={}",
        make_comparison_key(
            "small-files",
            args.client,
            args.backend,
            args.ciphers.as_deref(),
            &[args.total_size_mb],
            &[OperationKind::Roundtrip],
            std::slice::from_ref(&plan),
            profile_mode,
            args.sftp_requests,
            args.sftp_buffer_size,
        ),
        args.files
    );
    let previous = find_previous_run(&ctx.results_root, &comparison_key)?;

    let manifest = RunManifest {
        run_id: run_id.clone(),
        timestamp_unix: unix_now()?,
        mode: mode.to_string(),
        client: args.client,
        backend: args.backend,
        ciphers: args.ciphers.clone(),
        sizes_mb: vec![args.total_size_mb],
        operations: vec![OperationKind::Roundtrip],
        comparison_key: comparison_key.clone(),
        profile_mode,
        profile_tool: profile_mode.map(profile_tool_name),
        matrix_baseline: None,
        valid_for_comparison: true,
        invalid_reason: None,
        notes: args.note.clone(),
        machine: machine_metadata(),
        targets: vec![TargetManifest::from(&plan)],
    };
    write_json(run_dir.join("manifest.json"), &manifest)?;

    let bench_client_bin = if args.client == ClientKind::Bench {
        Some(build_bench_client(ctx, args.no_sccache)?)
    } else {
        None
    };
    let server_binary = build_server(ctx, &plan, args.no_sccache, profile_mode)?;
    let server_log = run_dir.join("server-current-small-files.log");
    let port = pick_free_port()?;
    let mut server = start_server(
        &server_binary,
        &server_log,
        &ctx.keys,
        port,
        args.backend,
        profile_mode,
        999,
        &artifact_dir,
        &format!("current-small-files-{}", client_name(args.client)),
    )?;
    wait_for_server(port, &ctx.keys.private_key, &args.ciphers)?;

    let warmup = args.warmup.unwrap_or(if args.profile { 0 } else { 2 });
    let runs = args.runs.unwrap_or(if args.profile { 1 } else { 10 });
    let iterations = match args.client {
        ClientKind::Openssh => {
            for iteration in 0..warmup {
                let _ = run_small_files_iteration(SmallFilesIteration {
                    port,
                    keys: &ctx.keys,
                    ciphers: args.ciphers.as_deref(),
                    sftp_requests: args.sftp_requests,
                    sftp_buffer_size: args.sftp_buffer_size,
                    file_paths: &file_paths,
                    artifact_dir: &artifact_dir,
                    iteration_label: &format!("warmup-{iteration}"),
                })?;
            }

            let mut iterations = Vec::new();
            for iteration in 0..runs {
                iterations.push(run_small_files_iteration(SmallFilesIteration {
                    port,
                    keys: &ctx.keys,
                    ciphers: args.ciphers.as_deref(),
                    sftp_requests: args.sftp_requests,
                    sftp_buffer_size: args.sftp_buffer_size,
                    file_paths: &file_paths,
                    artifact_dir: &artifact_dir,
                    iteration_label: &format!("run-{iteration}"),
                })?);
            }
            iterations
        }
        ClientKind::Bench => run_small_files_bench_client(
            bench_client_bin
                .as_deref()
                .expect("bench client binary should exist"),
            &args,
            warmup,
            runs,
            port,
            &artifact_dir,
        )?,
    };
    stop_server(&mut server)?;

    let results = RunResults {
        run_id: run_id.clone(),
        comparison_key,
        records: vec![MeasurementRecord::new_small_files(
            "current".to_string(),
            args.total_size_mb,
            total_bytes
                .checked_mul(2)
                .ok_or("roundtrip byte count overflow")?,
            (args.files as u64) * 2,
            iterations,
        )],
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
            file_count_per_iteration: None,
            iterations_seconds,
            mean_seconds,
            stddev_seconds,
            min_seconds,
            max_seconds,
            throughput_mib_per_second,
            file_ops_per_second: None,
        }
    }

    fn new_small_files(
        target_label: String,
        size_mb: u64,
        bytes_per_iteration: u64,
        files_per_iteration: u64,
        iterations_seconds: Vec<f64>,
    ) -> Self {
        let mut record = Self::new(
            target_label,
            OperationKind::Roundtrip,
            size_mb,
            bytes_per_iteration,
            iterations_seconds,
        );
        record.file_count_per_iteration = Some(files_per_iteration);
        record.file_ops_per_second = Some(files_per_iteration as f64 / record.mean_seconds);
        record
    }
}

fn make_build_plan(ctx: &AppContext, target: &TargetSpec) -> Result<BuildPlan, BoxError> {
    match &target.source {
        SourceSpec::Current { snapshot } => Ok(BuildPlan {
            label: target.label.clone(),
            manifest_source: ctx.repo_root.clone(),
            build_id: current_build_id(&target.label, &target.features),
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

fn current_build_id(label: &str, features: &[String]) -> String {
    format!(
        "current-{}-{}",
        sanitize_for_path(label),
        feature_build_suffix(features)
    )
}

fn feature_build_suffix(features: &[String]) -> String {
    if features.is_empty() {
        "default-features".to_string()
    } else {
        sanitize_for_path(&features.join(","))
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

fn run_small_files_bench_client(
    binary: &Path,
    args: &SmallFilesArgs,
    warmup: u32,
    runs: u32,
    port: u16,
    artifact_dir: &Path,
) -> Result<Vec<f64>, BoxError> {
    if warmup > 0 {
        let warmup_path = artifact_dir.join("current-small-files-bench-warmup.json");
        invoke_small_files_bench_client(binary, args, warmup, port, &warmup_path)?;
    }

    let output_path = artifact_dir.join("current-small-files-bench.json");
    let output = invoke_small_files_bench_client(binary, args, runs, port, &output_path)?;
    Ok(output
        .results
        .iter()
        .map(|result| result.total_seconds)
        .collect())
}

fn invoke_small_files_bench_client(
    binary: &Path,
    args: &SmallFilesArgs,
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
        .arg("roundtrip")
        .arg("--size")
        .arg(format!("{}MiB", args.total_size_mb))
        .arg("--files")
        .arg(args.files.to_string())
        .arg("--iterations")
        .arg(iterations.to_string())
        .arg("--chunk-size")
        .arg(&args.chunk_size)
        .arg("--file-depth")
        .arg(args.file_depth.to_string())
        .arg("--insecure")
        .arg("--json-output")
        .arg(output_path);
    if let Some(ciphers) = &args.ciphers {
        command.arg("--ciphers").arg(ciphers);
    }
    run_status(&mut command, "run sftp-bench-client small-files")?;
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
                common.sftp_requests,
                common.sftp_buffer_size,
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
                common.sftp_requests,
                common.sftp_buffer_size,
                &[format!("rm {}", remote_file), "quit".to_string()],
            );
            Ok(elapsed)
        }
        OperationKind::Download => {
            run_sftp_batch(
                port,
                keys,
                common.ciphers.as_deref(),
                common.sftp_requests,
                common.sftp_buffer_size,
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
                common.sftp_requests,
                common.sftp_buffer_size,
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
                common.sftp_requests,
                common.sftp_buffer_size,
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
                common.sftp_requests,
                common.sftp_buffer_size,
                &[
                    format!("put {} {}", test_file.display(), remote_file),
                    "quit".to_string(),
                ],
            )?;
            run_sftp_batch(
                port,
                keys,
                common.ciphers.as_deref(),
                common.sftp_requests,
                common.sftp_buffer_size,
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
                common.sftp_requests,
                common.sftp_buffer_size,
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
) -> Result<RunningServer, BoxError> {
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
        Some(ProfileKind::Perf) if cfg!(target_os = "macos") => {
            let trace_path = artifact_dir.join(format!("{artifact_prefix}.xctrace.trace"));
            let target_pid_path =
                artifact_dir.join(format!("{artifact_prefix}.xctrace-target.pid"));
            let mut command = Command::new("xctrace");
            command
                .arg("record")
                .arg("--quiet")
                .arg("--template")
                .arg("CPU Profiler")
                .arg("--output")
                .arg(&trace_path)
                .arg("--no-prompt")
                .arg("--launch")
                .arg("--")
                .arg("/bin/sh")
                .arg("-c")
                .arg(
                    r#"
printf '%s\n' "$$" > "$SFTP_PERF_XCTRACE_TARGET_PID"
exec "$@"
"#,
                )
                .arg("sftp-perf-xctrace-launch")
                .arg(binary);
            command.args(&args);
            command.env("SFTP_PERF_XCTRACE_TARGET_PID", &target_pid_path);
            command
        }
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
    let child = command.spawn()?;
    let kind = if matches!(profile_mode, Some(ProfileKind::Perf)) && cfg!(target_os = "macos") {
        RunningServerKind::XctraceLaunch {
            trace_path: artifact_dir.join(format!("{artifact_prefix}.xctrace.trace")),
            export_path: artifact_dir.join(format!("{artifact_prefix}.xctrace.xml")),
            target_pid_path: artifact_dir.join(format!("{artifact_prefix}.xctrace-target.pid")),
        }
    } else if matches!(profile_mode, Some(ProfileKind::Perf)) {
        RunningServerKind::PerfRecord
    } else {
        RunningServerKind::ServerProcess
    };
    Ok(RunningServer { child, kind })
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
            .arg(port.to_string());
        if let Some(cipher_list) = ciphers {
            command.arg("-c").arg(openssh_ciphers(cipher_list));
        }
        command
            .arg("benchmark@127.0.0.1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
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

struct RunningServer {
    child: Child,
    kind: RunningServerKind,
}

enum RunningServerKind {
    ServerProcess,
    PerfRecord,
    XctraceLaunch {
        trace_path: PathBuf,
        export_path: PathBuf,
        target_pid_path: PathBuf,
    },
}

fn stop_server(server: &mut RunningServer) -> Result<(), BoxError> {
    match &server.kind {
        RunningServerKind::ServerProcess => stop_child(&mut server.child),
        RunningServerKind::PerfRecord => stop_perf_record(&mut server.child),
        RunningServerKind::XctraceLaunch {
            trace_path,
            export_path,
            target_pid_path,
        } => {
            wait_for_path(target_pid_path, Duration::from_secs(10))?;
            kill_pid(read_pid(target_pid_path)?)?;
            let status = wait_for_exit(&mut server.child, Duration::from_secs(60))?;
            #[cfg(unix)]
            let interrupted = status.signal() == Some(2);
            #[cfg(not(unix))]
            let interrupted = false;
            // xctrace returns 54 when the launched target is killed, but still
            // writes a complete trace bundle after finalizing the recording.
            if !status.success()
                && !interrupted
                && status.code() != Some(130)
                && status.code() != Some(143)
                && status.code() != Some(54)
            {
                return Err(format!("xctrace exited unsuccessfully: {status}").into());
            }
            if let Err(error) = export_xctrace(trace_path, export_path) {
                eprintln!(
                    "warning: failed to export xctrace XML for {}: {error}",
                    trace_path.display()
                );
            }
            Ok(())
        }
    }
}

fn stop_perf_record(child: &mut Child) -> Result<(), BoxError> {
    if child.try_wait()?.is_none() {
        interrupt_pid(child.id())?;
    }
    let status = wait_for_exit(child, Duration::from_secs(60))?;
    #[cfg(unix)]
    let interrupted = matches!(status.signal(), Some(2 | 15));
    #[cfg(not(unix))]
    let interrupted = false;
    if status.success() || interrupted || status.code() == Some(130) || status.code() == Some(143) {
        Ok(())
    } else {
        Err(format!("perf record exited unsuccessfully: {status}").into())
    }
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
    sftp_requests: Option<u32>,
    sftp_buffer_size: Option<usize>,
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
        .arg(port.to_string());
    if let Some(cipher_list) = ciphers {
        command.arg("-c").arg(openssh_ciphers(cipher_list));
    }
    if let Some(requests) = sftp_requests {
        command.arg("-R").arg(requests.to_string());
    }
    if let Some(buffer_size) = sftp_buffer_size {
        command.arg("-B").arg(buffer_size.to_string());
    }
    command
        .arg("benchmark@127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        for line in commands {
            if let Err(error) = stdin.write_all(line.as_bytes()) {
                let status = child.wait()?;
                return Err(format!(
                    "sftp batch stdin failed after child exited with {status}: {error}"
                )
                .into());
            }
            if let Err(error) = stdin.write_all(b"\n") {
                let status = child.wait()?;
                return Err(format!(
                    "sftp batch stdin failed after child exited with {status}: {error}"
                )
                .into());
            }
        }
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("sftp batch failed with {status}").into())
    }
}

struct SmallFilesIteration<'a> {
    port: u16,
    keys: &'a KeyMaterial,
    ciphers: Option<&'a str>,
    sftp_requests: Option<u32>,
    sftp_buffer_size: Option<usize>,
    file_paths: &'a [PathBuf],
    artifact_dir: &'a Path,
    iteration_label: &'a str,
}

fn run_small_files_iteration(args: SmallFilesIteration<'_>) -> Result<f64, BoxError> {
    let SmallFilesIteration {
        port,
        keys,
        ciphers,
        sftp_requests,
        sftp_buffer_size,
        file_paths,
        artifact_dir,
        iteration_label,
    } = args;
    let remote_prefix = format!("small-files-{iteration_label}");
    let download_dir = artifact_dir.join(format!("download-{iteration_label}"));
    if download_dir.exists() {
        fs::remove_dir_all(&download_dir)?;
    }
    fs::create_dir_all(&download_dir)?;

    let mut commands = Vec::with_capacity(file_paths.len() * 3 + 1);
    for path in file_paths {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("invalid small-files fixture path {}", path.display()))?;
        commands.push(format!(
            "put {} {remote_prefix}-{file_name}",
            path.display()
        ));
    }
    for path in file_paths {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("invalid small-files fixture path {}", path.display()))?;
        commands.push(format!(
            "get {remote_prefix}-{file_name} {}",
            download_dir.join(file_name).display()
        ));
    }
    for path in file_paths {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("invalid small-files fixture path {}", path.display()))?;
        commands.push(format!("rm {remote_prefix}-{file_name}"));
    }
    commands.push("quit".to_string());

    let batch_path = artifact_dir.join(format!("small-files-{iteration_label}.sftp"));
    fs::write(&batch_path, commands.join("\n"))?;
    let start = Instant::now();
    let result = run_sftp_batch(
        port,
        keys,
        ciphers,
        sftp_requests,
        sftp_buffer_size,
        &commands,
    );
    let elapsed = start.elapsed().as_secs_f64();
    let _ = fs::remove_dir_all(&download_dir);
    result?;
    Ok(elapsed)
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
    if let Some(tool) = &manifest.profile_tool {
        output.push_str(&format!("Profiler: {tool}\n"));
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
                if let Some(file_ops) = record.file_ops_per_second {
                    output.push_str(&format!(
                        "             files {:8.1}/s  count {}\n",
                        file_ops,
                        record.file_count_per_iteration.unwrap_or_default()
                    ));
                }
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

fn ensure_small_files_fixture(
    cache_root: &Path,
    total_size_mb: u64,
    files: usize,
) -> Result<Vec<PathBuf>, BoxError> {
    let total_bytes = mib_to_bytes(total_size_mb)?;
    let dir = cache_root
        .join("small-files")
        .join(format!("{files}-files-{total_size_mb}mib"));
    let sizes = varied_file_sizes(total_bytes, files)?;
    let paths = (0..files)
        .map(|index| dir.join(format!("file-{index:05}.bin")))
        .collect::<Vec<_>>();
    let marker = dir.join(".fixture");
    if marker.exists() && paths.iter().all(|path| path.exists()) {
        return Ok(paths);
    }

    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;
    for (path, size) in paths.iter().zip(sizes) {
        let file = fs::File::create(path)?;
        file.set_len(size)?;
    }
    fs::write(
        marker,
        format!("files={files}\ntotal_size_mb={total_size_mb}\n"),
    )?;
    Ok(paths)
}

fn varied_file_sizes(total_bytes: u64, files: usize) -> Result<Vec<u64>, BoxError> {
    if files == 0 {
        return Err("--files must be greater than zero".into());
    }
    if total_bytes < files as u64 {
        return Err("total payload must be at least one byte per file".into());
    }

    let mut weights = Vec::with_capacity(files);
    for index in 0..files {
        let weight = 1 + (((index as u64) * 1_103_515_245 + 12_345) % 65_536);
        weights.push(weight);
    }
    let weight_sum = weights.iter().sum::<u64>();
    let remaining = total_bytes - files as u64;
    let mut sizes = weights
        .iter()
        .map(|weight| 1 + (remaining * *weight / weight_sum))
        .collect::<Vec<_>>();
    let mut allocated = sizes.iter().sum::<u64>();
    let mut index = 0;
    while allocated < total_bytes {
        sizes[index] += 1;
        allocated += 1;
        index = (index + 1) % sizes.len();
    }
    Ok(sizes)
}

fn ensure_small_files_args(args: &SmallFilesArgs) -> Result<(), BoxError> {
    if args.total_size_mb == 0 {
        return Err("--total-size-mb must be greater than zero".into());
    }
    if args.files == 0 {
        return Err("--files must be greater than zero".into());
    }
    let total_bytes = mib_to_bytes(args.total_size_mb)?;
    if total_bytes < args.files as u64 {
        return Err("--total-size-mb must allow at least one byte per file".into());
    }
    if args.file_depth == 0 {
        return Err("--file-depth must be greater than zero".into());
    }
    if args.client != ClientKind::Openssh {
        if args.sftp_requests.is_some() {
            return Err("--sftp-requests only applies to --client openssh".into());
        }
        if args.sftp_buffer_size.is_some() {
            return Err("--sftp-buffer-size only applies to --client openssh".into());
        }
    }
    Ok(())
}

fn ensure_common_args(args: &CommonArgs) -> Result<(), BoxError> {
    if args.client != ClientKind::Openssh {
        if args.sftp_requests.is_some() {
            return Err("--sftp-requests only applies to --client openssh".into());
        }
        if args.sftp_buffer_size.is_some() {
            return Err("--sftp-buffer-size only applies to --client openssh".into());
        }
    }
    Ok(())
}

fn mib_to_bytes(size_mb: u64) -> Result<u64, BoxError> {
    size_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "size is too large".into())
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
    sftp_requests: Option<u32>,
    sftp_buffer_size: Option<usize>,
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
        "mode={mode};client={};backend={};ciphers={};sizes={sizes};ops={operations};targets={target_labels};profile={};sftp_requests={};sftp_buffer_size={}",
        client_name(client),
        backend_name(backend),
        ciphers.unwrap_or("default"),
        profile_mode
            .map(profile_tool_name)
            .unwrap_or_else(|| "none".to_string()),
        sftp_requests
            .map(|requests| requests.to_string())
            .unwrap_or_else(|| "default".to_string()),
        sftp_buffer_size
            .map(|buffer_size| buffer_size.to_string())
            .unwrap_or_else(|| "default".to_string()),
    )
}

fn profile_tool_name(mode: ProfileKind) -> String {
    match mode {
        ProfileKind::Perf if cfg!(target_os = "macos") => "xctrace".to_string(),
        ProfileKind::Perf => "perf".to_string(),
        ProfileKind::Heaptrack => "heaptrack".to_string(),
    }
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
