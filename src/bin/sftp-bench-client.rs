use bytes::Bytes;
use clap::{Parser, ValueEnum};
use futures::future::try_join_all;
use futures::stream::{FuturesUnordered, StreamExt};
use russh::{client, ChannelId, Preferred};
use russh_sftp::client::RawSftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use sftp_s3::{parse_cipher, AVAILABLE_CIPHERS};
use std::borrow::Cow;
use std::{
    io,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const DEFAULT_READ_DEPTH: usize = 64;
const DEFAULT_WRITE_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, ValueEnum)]
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
}

struct AcceptAnyServerKey;

impl client::Handler for AcceptAnyServerKey {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
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
        sftp.close_session()
            .map_err(|error| boxed(format!("failed to close SFTP session: {error}")))?;
        return Ok(());
    }

    let results = match cli.operation {
        Operation::Upload => run_upload_benchmark(&sftp, &cli, &payload, &run_id).await?,
        Operation::Download => run_download_benchmark(&sftp, &cli, &payload, &run_id).await?,
        Operation::Roundtrip => run_roundtrip_benchmark(&sftp, &cli, &payload, &run_id).await?,
    };

    print_summary(&results, cli.size, cli.operation);
    sftp.close_session()
        .map_err(|error| boxed(format!("failed to close SFTP session: {error}")))?;
    Ok(())
}

async fn connect_sftp(addr: &str, cli: &Cli) -> Result<RawSftpSession, BoxError> {
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

    let mut session = client::connect(Arc::new(config), addr, AcceptAnyServerKey)
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

    let sftp = RawSftpSession::new(channel.into_stream());
    sftp.init()
        .await
        .map_err(|error| boxed(format!("failed to initialize SFTP session: {error}")))?;
    Ok(sftp)
}

async fn run_upload_benchmark(
    sftp: &Arc<RawSftpSession>,
    cli: &Cli,
    payload: &Arc<Vec<u8>>,
    run_id: &str,
) -> Result<Vec<IterationResult>, BoxError> {
    let mut results = Vec::with_capacity(cli.iterations as usize);

    for iteration in 0..cli.iterations {
        let paths = iteration_paths(cli, run_id, iteration);
        let upload = upload_paths(sftp, &paths, cli.size, payload, cli.write_depth).await?;

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
    sftp: &Arc<RawSftpSession>,
    cli: &Cli,
    payload: &Arc<Vec<u8>>,
    run_id: &str,
) -> Result<Vec<IterationResult>, BoxError> {
    let paths = iteration_paths(cli, run_id, 0);
    if !cli.skip_download_setup {
        upload_paths(sftp, &paths, cli.size, payload, cli.write_depth).await?;
    }

    let mut results = Vec::with_capacity(cli.iterations as usize);
    for iteration in 0..cli.iterations {
        let download =
            download_paths(sftp, &paths, cli.chunk_size, cli.size, cli.read_depth).await?;
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
    sftp: &Arc<RawSftpSession>,
    cli: &Cli,
    payload: &Arc<Vec<u8>>,
    run_id: &str,
) -> Result<(), BoxError> {
    let paths = iteration_paths(cli, run_id, 0);
    upload_paths(sftp, &paths, cli.size, payload, cli.write_depth).await?;
    Ok(())
}

async fn run_roundtrip_benchmark(
    sftp: &Arc<RawSftpSession>,
    cli: &Cli,
    payload: &Arc<Vec<u8>>,
    run_id: &str,
) -> Result<Vec<IterationResult>, BoxError> {
    let mut results = Vec::with_capacity(cli.iterations as usize);

    for iteration in 0..cli.iterations {
        let paths = iteration_paths(cli, run_id, iteration);
        let upload = upload_paths(sftp, &paths, cli.size, payload, cli.write_depth).await?;
        let download =
            download_paths(sftp, &paths, cli.chunk_size, cli.size, cli.read_depth).await?;

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
    sftp: &Arc<RawSftpSession>,
    paths: &[String],
    total_size: u64,
    payload: &Arc<Vec<u8>>,
    write_depth: usize,
) -> Result<Duration, BoxError> {
    let per_file_sizes = split_sizes(total_size, paths.len());
    let start = Instant::now();
    try_join_all(
        paths
            .iter()
            .cloned()
            .zip(per_file_sizes)
            .map(|(path, file_size)| {
                let sftp = Arc::clone(sftp);
                let payload = Arc::clone(payload);
                async move { upload_path(&sftp, path, file_size, &payload, write_depth).await }
            }),
    )
    .await?;

    Ok(start.elapsed())
}

async fn upload_path(
    sftp: &Arc<RawSftpSession>,
    path: String,
    file_size: u64,
    payload: &[u8],
    write_depth: usize,
) -> Result<(), BoxError> {
    let handle = sftp
        .open(
            path.clone(),
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            FileAttributes::empty(),
        )
        .await
        .map_err(|error| boxed(format!("failed to create {path}: {error}")))?
        .handle;
    let mut next_offset = 0_u64;
    let mut writes = FuturesUnordered::new();
    let write_depth = write_depth.max(1);

    while next_offset < file_size || !writes.is_empty() {
        while next_offset < file_size && writes.len() < write_depth {
            let remaining = (file_size - next_offset) as usize;
            let chunk_len = remaining.min(payload.len());
            writes.push(sftp.write_bytes(
                handle.clone(),
                next_offset,
                Bytes::copy_from_slice(&payload[..chunk_len]),
            ));
            next_offset += chunk_len as u64;
        }

        if let Some(result) = writes.next().await {
            result.map_err(|error| boxed(format!("failed to write remote file: {error}")))?;
        }
    }

    sftp.close_bytes(handle)
        .await
        .map_err(|error| boxed(format!("failed to close remote file: {error}")))?;

    Ok(())
}

async fn download_paths(
    sftp: &Arc<RawSftpSession>,
    paths: &[String],
    chunk_size: usize,
    total_size: u64,
    read_depth: usize,
) -> Result<Duration, BoxError> {
    let per_file_sizes = split_sizes(total_size, paths.len());

    let start = Instant::now();
    try_join_all(
        paths
            .iter()
            .cloned()
            .zip(per_file_sizes)
            .map(|(path, file_size)| {
                let sftp = Arc::clone(sftp);
                async move { download_path(&sftp, path, file_size, chunk_size, read_depth).await }
            }),
    )
    .await?;

    Ok(start.elapsed())
}

async fn download_path(
    sftp: &Arc<RawSftpSession>,
    path: String,
    file_size: u64,
    chunk_size: usize,
    read_depth: usize,
) -> Result<(), BoxError> {
    let handle = sftp
        .open(path.clone(), OpenFlags::READ, FileAttributes::empty())
        .await
        .map_err(|error| boxed(format!("failed to open {path}: {error}")))?
        .handle;
    let mut next_offset = 0_u64;
    let mut reads = FuturesUnordered::new();
    let read_depth = read_depth.max(1);
    let chunk_size = chunk_size.min(u32::MAX as usize);

    while next_offset < file_size || !reads.is_empty() {
        while next_offset < file_size && reads.len() < read_depth {
            let len = (file_size - next_offset).min(chunk_size as u64) as u32;
            reads.push(sftp.read_bytes(handle.clone(), next_offset, len));
            next_offset += u64::from(len);
        }

        if let Some(result) = reads.next().await {
            let data =
                result.map_err(|error| boxed(format!("failed to read remote file: {error}")))?;
            if data.data.is_empty() {
                break;
            }
        }
    }

    sftp.close_bytes(handle)
        .await
        .map_err(|error| boxed(format!("failed to close remote file: {error}")))?;

    Ok::<(), BoxError>(())
}

async fn cleanup_paths(sftp: &Arc<RawSftpSession>, paths: &[String]) {
    for path in paths {
        let _ = sftp.remove(path.clone()).await;
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

fn print_summary(results: &[IterationResult], bytes: u64, operation: Operation) {
    println!();
    println!("summary:");

    match operation {
        Operation::Upload => {
            let durations = results.iter().filter_map(|result| result.upload);
            print_average("upload", bytes, durations);
        }
        Operation::Download => {
            let durations = results.iter().filter_map(|result| result.download);
            print_average("download", bytes, durations);
        }
        Operation::Roundtrip => {
            let uploads = results.iter().filter_map(|result| result.upload);
            let downloads = results.iter().filter_map(|result| result.download);
            let totals = results.iter().map(IterationResult::total);

            print_average("upload", bytes, uploads);
            print_average("download", bytes, downloads);
            print_average("roundtrip", bytes * 2, totals);
        }
    }
}

fn print_average(label: &str, bytes: u64, durations: impl Iterator<Item = Duration>) {
    let durations = durations.collect::<Vec<_>>();
    if durations.is_empty() {
        return;
    }

    let total_secs = durations.iter().map(Duration::as_secs_f64).sum::<f64>();
    let average = Duration::from_secs_f64(total_secs / durations.len() as f64);

    println!(
        "{label:>9}: {:>10} avg over {} run(s), avg time {:.3}s",
        format_rate(bytes, average),
        durations.len(),
        average.as_secs_f64()
    );
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

    Ok(())
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
