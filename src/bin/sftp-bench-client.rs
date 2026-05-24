use bytes::Bytes;
use clap::{Parser, ValueEnum};
use futures::future::LocalBoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use futures::FutureExt;
use russh::{client, ChannelId, Preferred};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use serde::Serialize;
use sftp_s3::{parse_cipher, AVAILABLE_CIPHERS};
use std::borrow::Cow;
use std::{
    fs, io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type PendingRead =
    LocalBoxFuture<'static, (u64, u32, Result<Bytes, russh_sftp::client::error::Error>)>;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const DEFAULT_READ_DEPTH: usize = 64;
const DEFAULT_WRITE_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Upload,
    Download,
    Roundtrip,
}

#[derive(Parser, Debug)]
#[command(name = "sftp-bench-client")]
#[command(about = "SFTP client benchmark tool for sftp-s3 servers")]
struct Cli {
    /// SFTP server host
    #[arg(long, env = "SFTP_BENCH_HOST", default_value = "127.0.0.1")]
    host: String,

    /// SFTP server port
    #[arg(short, long, env = "SFTP_BENCH_PORT", default_value_t = 2222)]
    port: u16,

    /// Username for password authentication
    #[arg(short, long, env = "SFTP_BENCH_USER", default_value = "benchmark")]
    user: String,

    /// Password for password authentication
    #[arg(
        short = 'P',
        long,
        env = "SFTP_BENCH_PASSWORD",
        default_value = "benchmark"
    )]
    password: String,

    /// Remote directory to place benchmark files in
    #[arg(long, env = "SFTP_BENCH_DIR", default_value = ".")]
    remote_dir: String,

    /// Operation to benchmark
    #[arg(short, long, value_enum, default_value_t = Operation::Roundtrip)]
    operation: Operation,

    /// Total bytes per measured iteration, accepts suffixes B, KiB, MiB, GiB
    #[arg(short, long, default_value = "64MiB", value_parser = parse_size)]
    size: u64,

    /// Number of concurrent files to split each iteration across
    #[arg(short = 'n', long, default_value_t = 1)]
    files: usize,

    /// Number of measured iterations
    #[arg(short, long, default_value_t = 3)]
    iterations: u64,

    /// Stable identifier used in benchmark file names
    #[arg(long, env = "SFTP_BENCH_RUN_ID")]
    run_id: Option<String>,

    /// Read buffer size for downloads, accepts suffixes B, KiB, MiB, GiB
    #[arg(long, default_value = "256KiB", value_parser = parse_size_usize)]
    chunk_size: usize,

    /// Number of concurrent SFTP read requests per file
    #[arg(long, default_value_t = DEFAULT_READ_DEPTH)]
    read_depth: usize,

    /// Number of concurrent SFTP write requests per file
    #[arg(long, default_value_t = DEFAULT_WRITE_DEPTH)]
    write_depth: usize,

    /// Number of files to process concurrently
    #[arg(long, default_value_t = 1)]
    file_depth: usize,

    /// Per-request SFTP timeout in seconds; 0 disables request timeouts
    #[arg(long, default_value_t = 0)]
    request_timeout: u64,

    /// Leave benchmark files on the server
    #[arg(long)]
    keep_files: bool,

    /// For download benchmarks, upload the fixture and exit without measuring
    #[arg(long)]
    prepare_only: bool,

    /// For download benchmarks, reuse an existing fixture instead of uploading it first
    #[arg(long)]
    skip_download_setup: bool,

    /// Set TCP_NODELAY on the SSH connection
    #[arg(long)]
    nodelay: bool,

    /// Preferred ciphers (comma-separated, in order of preference)
    /// Available: aes256-gcm, aes128-ctr, aes256-ctr, chacha20-poly1305
    #[arg(long, env = "SFTP_BENCH_CIPHERS", value_delimiter = ',')]
    ciphers: Option<Vec<String>>,

    /// Accept any SSH host key without verification
    #[arg(long, env = "SFTP_BENCH_INSECURE")]
    insecure: bool,

    /// Expected SSH host public key (OpenSSH public-key line or base64 body)
    #[arg(long, env = "SFTP_BENCH_HOST_KEY")]
    host_key: Option<String>,

    /// Path to a file containing the expected SSH host public key
    #[arg(long, env = "SFTP_BENCH_HOST_KEY_FILE")]
    host_key_file: Option<PathBuf>,

    /// Write structured benchmark results to a JSON file
    #[arg(long)]
    json_output: Option<PathBuf>,
}

enum HostKeyVerifier {
    Insecure,
    Pinned(russh::keys::PublicKey),
}

impl client::Handler for HostKeyVerifier {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(match self {
            Self::Insecure => true,
            Self::Pinned(expected) => server_public_key == expected,
        })
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Debug)]
struct IterationResult {
    upload: Option<Duration>,
    download: Option<Duration>,
}

impl IterationResult {
    fn total(&self) -> Duration {
        self.upload.unwrap_or_default() + self.download.unwrap_or_default()
    }
}

#[derive(Serialize)]
struct JsonIterationResult {
    upload_seconds: Option<f64>,
    download_seconds: Option<f64>,
    total_seconds: f64,
}

#[derive(Clone, Serialize)]
struct JsonSummaryEntry {
    label: &'static str,
    bytes: u64,
    average_seconds: f64,
    average_mib_per_second: f64,
    runs: usize,
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    operation: Operation,
    bytes: u64,
    iterations: u64,
    run_id: &'a str,
    results: Vec<JsonIterationResult>,
    summary: Vec<JsonSummaryEntry>,
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let cli = Cli::parse();
    validate_cli(&cli)?;

    let addr = format!("{}:{}", cli.host, cli.port);
    let sftp = Arc::new(connect_sftp(&addr, &cli).await?);
    let run_id = cli.run_id.clone().unwrap_or_else(run_id);
    let payload = Arc::new(make_payload(
        cli.chunk_size.min(max_file_size(cli.size, cli.files)),
    ));

    println!("target:     {addr}");
    println!("user:       {}", cli.user);
    println!("operation:  {:?}", cli.operation);
    println!("total size: {}", format_bytes(cli.size));
    println!("files:      {}", cli.files);
    println!("iterations: {}", cli.iterations);
    println!("run id:     {run_id}");
    println!();

    if cli.prepare_only {
        prepare_download_fixture(&sftp, &cli, &payload, &run_id).await?;
        println!("prepared download fixture");
        sftp.close()
            .await
            .map_err(|error| boxed(format!("failed to close SFTP session: {error}")))?;
        return Ok(());
    }

    let results = match cli.operation {
        Operation::Upload => run_upload_benchmark(&sftp, &cli, &payload, &run_id).await?,
        Operation::Download => run_download_benchmark(&sftp, &cli, &payload, &run_id).await?,
        Operation::Roundtrip => run_roundtrip_benchmark(&sftp, &cli, &payload, &run_id).await?,
    };

    let summary = build_summary(&results, cli.size, cli.operation);
    print_summary(&summary);
    if let Some(path) = cli.json_output.as_ref() {
        write_json_output(
            path,
            &results,
            &summary,
            cli.size,
            cli.operation,
            cli.iterations,
            &run_id,
        )?;
    }
    sftp.close()
        .await
        .map_err(|error| boxed(format!("failed to close SFTP session: {error}")))?;
    Ok(())
}

async fn connect_sftp(addr: &str, cli: &Cli) -> Result<SftpSession, BoxError> {
    let mut config = client::Config {
        nodelay: cli.nodelay,
        ..Default::default()
    };
    if let Some(cipher_names) = cli.ciphers.as_ref() {
        let ciphers = parse_ciphers(cipher_names)?;
        let mut preferred = Preferred::DEFAULT;
        preferred.cipher = Cow::Owned(ciphers);
        config.preferred = preferred;
    }

    let mut session = client::connect(Arc::new(config), addr, build_host_key_verifier(cli)?)
        .await
        .map_err(|error| boxed(format!("failed to connect to {addr}: {error}")))?;

    let auth = session
        .authenticate_password(&cli.user, &cli.password)
        .await
        .map_err(|error| boxed(format!("failed to authenticate as {}: {error}", cli.user)))?;

    if !auth.success() {
        return Err(boxed(format!(
            "password authentication failed for {}",
            cli.user
        )));
    }

    let channel = session
        .channel_open_session()
        .await
        .map_err(|error| boxed(format!("failed to open SSH session channel: {error}")))?;

    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|error| boxed(format!("failed to request SFTP subsystem: {error}")))?;

    SftpSession::new_opts(channel.into_stream(), Some(cli.request_timeout))
        .await
        .map_err(|error| boxed(format!("failed to initialize SFTP session: {error}")))
}

async fn run_upload_benchmark(
    sftp: &Arc<SftpSession>,
    cli: &Cli,
    payload: &Arc<Vec<u8>>,
    run_id: &str,
) -> Result<Vec<IterationResult>, BoxError> {
    let mut results = Vec::with_capacity(cli.iterations as usize);

    for iteration in 0..cli.iterations {
        let paths = iteration_paths(cli, run_id, iteration);
        let upload = upload_paths(
            sftp,
            &paths,
            cli.size,
            payload,
            cli.write_depth,
            cli.file_depth,
        )
        .await?;

        if !cli.keep_files {
            cleanup_paths(sftp, &paths).await;
        }

        print_iteration(iteration, cli.size, Some(upload), None);
        results.push(IterationResult {
            upload: Some(upload),
            download: None,
        });
    }

    Ok(results)
}

async fn run_download_benchmark(
    sftp: &Arc<SftpSession>,
    cli: &Cli,
    payload: &Arc<Vec<u8>>,
    run_id: &str,
) -> Result<Vec<IterationResult>, BoxError> {
    let paths = iteration_paths(cli, run_id, 0);
    if !cli.skip_download_setup {
        upload_paths(
            sftp,
            &paths,
            cli.size,
            payload,
            cli.write_depth,
            cli.file_depth,
        )
        .await?;
    }

    let mut results = Vec::with_capacity(cli.iterations as usize);
    for iteration in 0..cli.iterations {
        let download = download_paths(
            sftp,
            &paths,
            cli.chunk_size,
            cli.size,
            cli.read_depth,
            cli.file_depth,
        )
        .await?;
        print_iteration(iteration, cli.size, None, Some(download));
        results.push(IterationResult {
            upload: None,
            download: Some(download),
        });
    }

    if !cli.keep_files {
        cleanup_paths(sftp, &paths).await;
    }

    Ok(results)
}

async fn prepare_download_fixture(
    sftp: &Arc<SftpSession>,
    cli: &Cli,
    payload: &Arc<Vec<u8>>,
    run_id: &str,
) -> Result<(), BoxError> {
    let paths = iteration_paths(cli, run_id, 0);
    upload_paths(
        sftp,
        &paths,
        cli.size,
        payload,
        cli.write_depth,
        cli.file_depth,
    )
    .await?;
    Ok(())
}

async fn run_roundtrip_benchmark(
    sftp: &Arc<SftpSession>,
    cli: &Cli,
    payload: &Arc<Vec<u8>>,
    run_id: &str,
) -> Result<Vec<IterationResult>, BoxError> {
    let mut results = Vec::with_capacity(cli.iterations as usize);

    for iteration in 0..cli.iterations {
        let paths = iteration_paths(cli, run_id, iteration);
        let upload = upload_paths(
            sftp,
            &paths,
            cli.size,
            payload,
            cli.write_depth,
            cli.file_depth,
        )
        .await?;
        let download = download_paths(
            sftp,
            &paths,
            cli.chunk_size,
            cli.size,
            cli.read_depth,
            cli.file_depth,
        )
        .await?;

        if !cli.keep_files {
            cleanup_paths(sftp, &paths).await;
        }

        print_iteration(iteration, cli.size, Some(upload), Some(download));
        results.push(IterationResult {
            upload: Some(upload),
            download: Some(download),
        });
    }

    Ok(results)
}

async fn upload_paths(
    sftp: &Arc<SftpSession>,
    paths: &[String],
    total_size: u64,
    payload: &Arc<Vec<u8>>,
    write_depth: usize,
    file_depth: usize,
) -> Result<Duration, BoxError> {
    let per_file_sizes = split_sizes(total_size, paths.len());
    let start = Instant::now();
    let mut uploads = futures::stream::iter(paths.iter().cloned().zip(per_file_sizes).map(
        |(path, file_size)| {
            let sftp = Arc::clone(sftp);
            let payload = Arc::clone(payload);
            async move { upload_path(&sftp, path, file_size, &payload, write_depth).await }
        },
    ))
    .buffer_unordered(file_depth.max(1));

    while let Some(result) = uploads.next().await {
        result?;
    }

    Ok(start.elapsed())
}

async fn upload_path(
    sftp: &Arc<SftpSession>,
    path: String,
    file_size: u64,
    payload: &[u8],
    write_depth: usize,
) -> Result<(), BoxError> {
    let file = sftp
        .open_with_flags(
            path.clone(),
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .map_err(|error| boxed(format!("failed to create {path}: {error}")))?;
    let file = Arc::new(file);
    let mut next_offset = 0_u64;
    let mut writes = FuturesUnordered::new();
    let write_depth = write_depth.max(1);

    while next_offset < file_size || !writes.is_empty() {
        while next_offset < file_size && writes.len() < write_depth {
            let remaining = (file_size - next_offset) as usize;
            let chunk_len = remaining.min(payload.len());
            let file = Arc::clone(&file);
            let data = Bytes::copy_from_slice(&payload[..chunk_len]);
            writes.push(async move { file.write_at(next_offset, data).await });
            next_offset += chunk_len as u64;
        }

        if let Some(result) = writes.next().await {
            result.map_err(|error| boxed(format!("failed to write remote file: {error}")))?;
        }
    }

    Arc::try_unwrap(file)
        .map_err(|_| boxed("write file still has outstanding references"))?
        .close()
        .await
        .map_err(|error| boxed(format!("failed to close remote file: {error}")))?;

    Ok(())
}

async fn download_paths(
    sftp: &Arc<SftpSession>,
    paths: &[String],
    chunk_size: usize,
    total_size: u64,
    read_depth: usize,
    file_depth: usize,
) -> Result<Duration, BoxError> {
    let per_file_sizes = split_sizes(total_size, paths.len());

    let start = Instant::now();
    let mut downloads = futures::stream::iter(paths.iter().cloned().zip(per_file_sizes).map(
        |(path, file_size)| {
            let sftp = Arc::clone(sftp);
            async move { download_path(&sftp, path, file_size, chunk_size, read_depth).await }
        },
    ))
    .buffer_unordered(file_depth.max(1));

    while let Some(result) = downloads.next().await {
        result?;
    }

    Ok(start.elapsed())
}

async fn download_path(
    sftp: &Arc<SftpSession>,
    path: String,
    file_size: u64,
    chunk_size: usize,
    read_depth: usize,
) -> Result<(), BoxError> {
    let file = sftp
        .open(path.clone())
        .await
        .map_err(|error| boxed(format!("failed to open {path}: {error}")))?;
    let file = Arc::new(file);
    let mut next_offset = 0_u64;
    let mut reads: FuturesUnordered<PendingRead> = FuturesUnordered::new();
    let read_depth = read_depth.max(1);
    let chunk_size = chunk_size.min(u32::MAX as usize);
    let mut bytes_received = 0_u64;

    while next_offset < file_size || !reads.is_empty() {
        while next_offset < file_size && reads.len() < read_depth {
            let request_offset = next_offset;
            let len = (file_size - next_offset).min(chunk_size as u64) as u32;
            let file = Arc::clone(&file);
            reads.push(
                async move {
                    let data = file.read_at(request_offset, len).await;
                    (request_offset, len, data)
                }
                .boxed_local(),
            );
            next_offset += u64::from(len);
        }

        if let Some((offset, requested_len, result)) = reads.next().await {
            let data =
                result.map_err(|error| boxed(format!("failed to read remote file: {error}")))?;
            let actual_len =
                u32::try_from(data.len()).map_err(|_| boxed("read chunk length overflow"))?;
            if actual_len == 0 {
                return Err(boxed(format!(
                    "unexpected EOF while reading {path} at offset {offset}"
                )));
            }

            bytes_received += u64::from(actual_len);
            if actual_len < requested_len {
                let retry_offset = offset + u64::from(actual_len);
                let retry_len = requested_len - actual_len;
                let file = Arc::clone(&file);
                reads.push(
                    async move {
                        let data = file.read_at(retry_offset, retry_len).await;
                        (retry_offset, retry_len, data)
                    }
                    .boxed_local(),
                );
            }
        }
    }

    if bytes_received != file_size {
        return Err(boxed(format!(
            "short read for {path}: expected {file_size} bytes, got {bytes_received}"
        )));
    }

    Arc::try_unwrap(file)
        .map_err(|_| boxed("read file still has outstanding references"))?
        .close()
        .await
        .map_err(|error| boxed(format!("failed to close remote file: {error}")))?;

    Ok::<(), BoxError>(())
}

async fn cleanup_paths(sftp: &Arc<SftpSession>, paths: &[String]) {
    for path in paths {
        let _ = sftp.remove_file(path.clone()).await;
    }
}

fn iteration_paths(cli: &Cli, run_id: &str, iteration: u64) -> Vec<String> {
    (0..cli.files)
        .map(|index| {
            remote_path(
                &cli.remote_dir,
                &format!("sftp-bench-client-{run_id}-{iteration}-{index}.bin"),
            )
        })
        .collect()
}

fn remote_path(remote_dir: &str, file_name: &str) -> String {
    match remote_dir.trim_end_matches('/') {
        "" | "." => file_name.to_string(),
        "/" => format!("/{file_name}"),
        dir => format!("{dir}/{file_name}"),
    }
}

fn split_sizes(total_size: u64, files: usize) -> Vec<u64> {
    let base = total_size / files as u64;
    let remainder = total_size % files as u64;

    (0..files)
        .map(|index| base + u64::from((index as u64) < remainder))
        .collect()
}

fn max_file_size(total_size: u64, files: usize) -> usize {
    let base = total_size / files as u64;
    let remainder = total_size % files as u64;
    (base + u64::from(remainder > 0)) as usize
}

fn make_payload(size: usize) -> Vec<u8> {
    let mut payload = vec![0; size];
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(31).wrapping_add(17);
    }
    payload
}

fn print_iteration(
    iteration: u64,
    bytes: u64,
    upload: Option<Duration>,
    download: Option<Duration>,
) {
    print!("iteration {:>3}:", iteration + 1);

    if let Some(elapsed) = upload {
        print!(" upload {:>10}", format_rate(bytes, elapsed));
    }

    if let Some(elapsed) = download {
        print!(" download {:>10}", format_rate(bytes, elapsed));
    }

    println!();
}

fn build_summary(
    results: &[IterationResult],
    bytes: u64,
    operation: Operation,
) -> Vec<JsonSummaryEntry> {
    let mut summary = Vec::new();
    let mut push_average = |label: &'static str, bytes: u64, durations: Vec<Duration>| {
        if let Some(entry) = average_entry(label, bytes, &durations) {
            summary.push(entry);
        }
    };

    match operation {
        Operation::Upload => {
            push_average(
                "upload",
                bytes,
                results.iter().filter_map(|result| result.upload).collect(),
            );
        }
        Operation::Download => {
            push_average(
                "download",
                bytes,
                results
                    .iter()
                    .filter_map(|result| result.download)
                    .collect(),
            );
        }
        Operation::Roundtrip => {
            push_average(
                "upload",
                bytes,
                results.iter().filter_map(|result| result.upload).collect(),
            );
            push_average(
                "download",
                bytes,
                results
                    .iter()
                    .filter_map(|result| result.download)
                    .collect(),
            );
            push_average(
                "roundtrip",
                bytes * 2,
                results.iter().map(IterationResult::total).collect(),
            );
        }
    }

    summary
}

fn print_summary(summary: &[JsonSummaryEntry]) {
    println!();
    println!("summary:");
    for entry in summary {
        println!(
            "{label:>9}: {:>10} avg over {} run(s), avg time {:.3}s",
            format_rate(entry.bytes, Duration::from_secs_f64(entry.average_seconds)),
            entry.runs,
            entry.average_seconds,
            label = entry.label,
        );
    }
}

fn average_entry(
    label: &'static str,
    bytes: u64,
    durations: &[Duration],
) -> Option<JsonSummaryEntry> {
    if durations.is_empty() {
        return None;
    }

    let total_secs = durations.iter().map(Duration::as_secs_f64).sum::<f64>();
    let average = Duration::from_secs_f64(total_secs / durations.len() as f64);
    let mib = bytes as f64 / MIB as f64;
    Some(JsonSummaryEntry {
        label,
        bytes,
        average_seconds: average.as_secs_f64(),
        average_mib_per_second: mib / average.as_secs_f64(),
        runs: durations.len(),
    })
}

fn write_json_output(
    path: &PathBuf,
    results: &[IterationResult],
    summary: &[JsonSummaryEntry],
    bytes: u64,
    operation: Operation,
    iterations: u64,
    run_id: &str,
) -> Result<(), BoxError> {
    let output = JsonOutput {
        operation,
        bytes,
        iterations,
        run_id,
        results: results
            .iter()
            .map(|result| JsonIterationResult {
                upload_seconds: result.upload.map(|elapsed| elapsed.as_secs_f64()),
                download_seconds: result.download.map(|elapsed| elapsed.as_secs_f64()),
                total_seconds: result.total().as_secs_f64(),
            })
            .collect(),
        summary: summary.to_vec(),
    };
    let json = serde_json::to_vec_pretty(&output)
        .map_err(|error| boxed(format!("failed to encode benchmark JSON: {error}")))?;
    fs::write(path, json).map_err(|error| {
        boxed(format!(
            "failed to write benchmark JSON to {}: {error}",
            path.display()
        ))
    })
}

fn format_rate(bytes: u64, elapsed: Duration) -> String {
    let mib = bytes as f64 / MIB as f64;
    let rate = mib / elapsed.as_secs_f64();
    format!("{rate:.2} MiB/s")
}

fn format_bytes(bytes: u64) -> String {
    if bytes.is_multiple_of(1024 * MIB) {
        format!("{} GiB", bytes / (1024 * MIB))
    } else if bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else if bytes.is_multiple_of(KIB) {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn validate_cli(cli: &Cli) -> Result<(), BoxError> {
    if cli.size == 0 {
        return Err(boxed("--size must be greater than zero"));
    }
    if cli.files == 0 {
        return Err(boxed("--files must be greater than zero"));
    }
    if cli.read_depth == 0 {
        return Err(boxed("--read-depth must be greater than zero"));
    }
    if cli.write_depth == 0 {
        return Err(boxed("--write-depth must be greater than zero"));
    }
    if cli.file_depth == 0 {
        return Err(boxed("--file-depth must be greater than zero"));
    }
    if cli.iterations == 0 {
        return Err(boxed("--iterations must be greater than zero"));
    }
    if cli.run_id.as_deref() == Some("") {
        return Err(boxed("--run-id must not be empty"));
    }
    if cli.prepare_only && !matches!(cli.operation, Operation::Download) {
        return Err(boxed("--prepare-only only applies to --operation download"));
    }
    if cli.skip_download_setup && !matches!(cli.operation, Operation::Download) {
        return Err(boxed(
            "--skip-download-setup only applies to --operation download",
        ));
    }
    if cli.prepare_only && cli.skip_download_setup {
        return Err(boxed(
            "--prepare-only and --skip-download-setup cannot be used together",
        ));
    }
    if cli.chunk_size == 0 {
        return Err(boxed("--chunk-size must be greater than zero"));
    }
    if cli.size > usize::MAX as u64 {
        return Err(boxed("--size is too large for this platform"));
    }
    if cli.insecure && (cli.host_key.is_some() || cli.host_key_file.is_some()) {
        return Err(boxed(
            "--insecure cannot be combined with --host-key or --host-key-file",
        ));
    }
    if !cli.insecure && cli.host_key.is_none() && cli.host_key_file.is_none() {
        return Err(boxed(
            "provide --host-key/--host-key-file or pass --insecure for local benchmarking",
        ));
    }

    Ok(())
}

fn build_host_key_verifier(cli: &Cli) -> Result<HostKeyVerifier, BoxError> {
    if cli.insecure {
        return Ok(HostKeyVerifier::Insecure);
    }

    let raw = if let Some(host_key) = cli.host_key.as_deref() {
        host_key.to_owned()
    } else if let Some(path) = cli.host_key_file.as_ref() {
        std::fs::read_to_string(path).map_err(|error| {
            boxed(format!(
                "failed to read host key file {}: {error}",
                path.display()
            ))
        })?
    } else {
        return Err(boxed("missing host key configuration"));
    };

    let key = parse_public_key(&raw).ok_or_else(|| boxed("failed to parse SSH host public key"))?;
    Ok(HostKeyVerifier::Pinned(key))
}

fn parse_public_key(raw: &str) -> Option<russh::keys::PublicKey> {
    raw.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        let encoded = if parts.len() >= 2 { parts[1] } else { parts[0] };
        russh::keys::parse_public_key_base64(encoded).ok()
    })
}

fn parse_ciphers(cipher_names: &[String]) -> Result<Vec<russh::cipher::Name>, BoxError> {
    let mut ciphers = Vec::with_capacity(cipher_names.len());

    for name in cipher_names {
        match parse_cipher(name) {
            Some(cipher) => ciphers.push(cipher),
            None => {
                return Err(boxed(format!(
                    "unknown cipher '{}'. Available: {}",
                    name,
                    AVAILABLE_CIPHERS.join(", ")
                )));
            }
        }
    }

    if ciphers.is_empty() {
        return Err(boxed("--ciphers must include at least one cipher"));
    }

    Ok(ciphers)
}

fn parse_size_usize(raw: &str) -> Result<usize, String> {
    let size = parse_size(raw)?;
    usize::try_from(size).map_err(|_| format!("size '{raw}' is too large for this platform"))
}

fn parse_size(raw: &str) -> Result<u64, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("size cannot be empty".to_string());
    }

    let lower = raw.to_ascii_lowercase();
    let (number, multiplier) = if let Some(number) = lower.strip_suffix("gib") {
        (number, 1024 * MIB)
    } else if let Some(number) = lower.strip_suffix("gb") {
        (number, 1_000_000_000)
    } else if let Some(number) = lower.strip_suffix("mib") {
        (number, MIB)
    } else if let Some(number) = lower.strip_suffix("mb") {
        (number, 1_000_000)
    } else if let Some(number) = lower.strip_suffix("kib") {
        (number, KIB)
    } else if let Some(number) = lower.strip_suffix("kb") {
        (number, 1_000)
    } else if let Some(number) = lower.strip_suffix('b') {
        (number, 1)
    } else {
        (lower.as_str(), 1)
    };

    let number = number
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("invalid size '{raw}': {error}"))?;

    number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size '{raw}' is too large"))
}

fn run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{}-{millis}", std::process::id())
}

fn boxed(message: impl Into<String>) -> BoxError {
    Box::new(io::Error::other(message.into()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn bench_client_uses_high_level_sftp_session_api() {
        let source = include_str!("sftp-bench-client.rs");

        assert!(source.contains("SftpSession::new_opts"));
        assert!(!source.contains(concat!("Raw", "SftpSession")));
        assert!(!source.contains(concat!("read", "_bytes")));
        assert!(!source.contains(concat!("write", "_bytes")));
        assert!(!source.contains(concat!("close", "_bytes")));
    }
}
