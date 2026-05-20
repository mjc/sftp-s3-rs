#![cfg(feature = "s3")]

//! Opt-in end-to-end SFTP tests against the S3 backend using MinIO.
//!
//! Run locally with:
//!   nix develop -c cargo test --all-features --test s3_minio_integration -- --ignored --nocapture

use aws_sdk_s3::Client;
use russh::client;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use sftp_s3::backend::Backend;
use sftp_s3::{S3Backend, S3Config, Server, ServerConfig};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};

const MINIO_ACCESS_KEY: &str = "minioadmin";
const MINIO_SECRET_KEY: &str = "minioadmin";
const REGION: &str = "us-east-1";
const USER: &str = "test";
const PASS: &str = "pass";

struct TestClient;

impl client::Handler for TestClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

#[tokio::test]
#[ignore = "starts a local MinIO process from nix develop"]
async fn test_sftp_roundtrip_against_minio_s3_backend() {
    let minio = MinioProcess::start().await;
    let endpoint = minio.endpoint.clone();
    configure_aws_env(&endpoint);

    let s3_client = minio_client(&endpoint).await;
    wait_for_minio(&s3_client).await;

    let bucket = unique_bucket_name();
    s3_client
        .create_bucket()
        .bucket(&bucket)
        .send()
        .await
        .expect("create MinIO bucket");

    let port = start_s3_test_server(&endpoint, &bucket, "sftp/").await;
    let sftp = connect_sftp(port).await;

    sftp.create_dir("/docs")
        .await
        .expect("create docs directory");

    let mut file = sftp
        .open_with_flags(
            "/docs/hello.txt",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .expect("open hello.txt for writing");
    file.write_all(b"hello from minio")
        .await
        .expect("write hello.txt");
    file.shutdown().await.expect("finish hello.txt write");

    let meta = sftp
        .metadata("/docs/hello.txt")
        .await
        .expect("stat hello.txt");
    assert_eq!(meta.size, Some("hello from minio".len() as u64));

    let direct_backend = S3Backend::with_endpoint(
        S3Config::new(&bucket).with_prefix("sftp/"),
        &endpoint,
        REGION,
    )
    .await;
    let direct_docs_info = direct_backend
        .file_info("docs")
        .await
        .expect("direct S3Backend docs file_info");
    assert!(direct_docs_info.is_dir, "docs should be a directory");

    let direct_docs_entries = direct_backend
        .list_dir("docs")
        .await
        .expect("direct S3Backend docs listing");
    let direct_docs_names: Vec<_> = direct_docs_entries
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert!(
        direct_docs_names.contains(&"hello.txt".to_string()),
        "expected direct backend listing to include hello.txt, got {direct_docs_names:?}"
    );

    let root_names: Vec<_> = sftp
        .read_dir("/")
        .await
        .expect("read root")
        .map(|entry| entry.file_name())
        .collect();
    assert!(
        root_names.contains(&"docs".to_string()),
        "expected docs in root listing, got {root_names:?}"
    );

    let docs_names: Vec<_> = sftp
        .read_dir("/docs")
        .await
        .expect("read docs")
        .map(|entry| entry.file_name())
        .collect();
    assert!(
        docs_names.contains(&"hello.txt".to_string()),
        "expected hello.txt in docs listing, got {docs_names:?}"
    );

    sftp.rename("/docs/hello.txt", "/docs/greeting.txt")
        .await
        .expect("rename hello.txt");

    let mut file = sftp
        .open_with_flags("/docs/greeting.txt", OpenFlags::READ)
        .await
        .expect("open greeting.txt for reading");
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .await
        .expect("read greeting.txt");
    assert_eq!(contents, "hello from minio");
    assert!(
        sftp.metadata("/docs/hello.txt").await.is_err(),
        "original path should be gone after rename"
    );

    let large = large_payload();
    let mut file = sftp
        .open_with_flags(
            "/large.bin",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .expect("open large.bin for writing");
    file.write_all(&large).await.expect("write large.bin");
    file.shutdown().await.expect("finish large.bin write");

    let mut file = sftp
        .open_with_flags("/large.bin", OpenFlags::READ)
        .await
        .expect("open large.bin for reading");
    let mut readback = Vec::new();
    file.read_to_end(&mut readback)
        .await
        .expect("read large.bin");
    assert_eq!(readback, large, "multipart readback should match upload");

    sftp.remove_file("/docs/greeting.txt")
        .await
        .expect("delete greeting.txt");
    sftp.remove_file("/large.bin")
        .await
        .expect("delete large.bin");
    sftp.remove_dir("/docs")
        .await
        .expect("delete docs directory");

    cleanup_bucket(&s3_client, &bucket).await;
}

struct MinioProcess {
    endpoint: String,
    child: Option<Child>,
    _data_dir: Option<tempfile::TempDir>,
}

impl MinioProcess {
    async fn start() -> Self {
        if let Ok(endpoint) = std::env::var("SFTP_S3_MINIO_ENDPOINT") {
            return Self {
                endpoint,
                child: None,
                _data_dir: None,
            };
        }

        let data_dir = tempfile::tempdir().expect("create MinIO data dir");
        let api_addr = free_local_addr().await;
        let console_addr = free_local_addr().await;
        let endpoint = format!("http://{api_addr}");

        let child = Command::new("minio")
            .arg("server")
            .arg("--address")
            .arg(&api_addr)
            .arg("--console-address")
            .arg(&console_addr)
            .arg(data_dir.path())
            .env("MINIO_ROOT_USER", MINIO_ACCESS_KEY)
            .env("MINIO_ROOT_PASSWORD", MINIO_SECRET_KEY)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("start minio; run through `nix develop` so the minio package is available");

        wait_for_tcp(&api_addr).await;

        Self {
            endpoint,
            child: Some(child),
            _data_dir: Some(data_dir),
        }
    }
}

impl Drop for MinioProcess {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

fn configure_aws_env(endpoint: &str) {
    std::env::set_var("AWS_ACCESS_KEY_ID", MINIO_ACCESS_KEY);
    std::env::set_var("AWS_SECRET_ACCESS_KEY", MINIO_SECRET_KEY);
    std::env::set_var("AWS_REGION", REGION);
    std::env::set_var("AWS_DEFAULT_REGION", REGION);
    std::env::set_var("AWS_ENDPOINT_URL", endpoint);
}

async fn minio_client(endpoint: &str) -> Client {
    let sdk_config = aws_config::from_env()
        .endpoint_url(endpoint)
        .region(aws_config::Region::new(REGION))
        .load()
        .await;
    let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config)
        .force_path_style(true)
        .build();
    Client::from_conf(s3_config)
}

async fn wait_for_minio(client: &Client) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if client.list_buckets().send().await.is_ok() {
            return;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "MinIO did not become ready within 30 seconds"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn free_local_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral local port");
    let addr = listener.local_addr().expect("read ephemeral local port");
    drop(listener);
    addr.to_string()
}

async fn wait_for_tcp(addr: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "MinIO did not open {addr} within 30 seconds"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn unique_bucket_name() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    format!("sftp-s3-e2e-{}-{now}", std::process::id())
}

fn large_payload() -> Vec<u8> {
    (0u8..=255).cycle().take(6 * 1024 * 1024).collect()
}

async fn start_s3_test_server(endpoint: &str, bucket: &str, prefix: &str) -> u16 {
    let backend =
        S3Backend::with_endpoint(S3Config::new(bucket).with_prefix(prefix), endpoint, REGION).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        Server::new(backend)
            .config(ServerConfig::new().with_generated_key())
            .with_users(vec![(USER.to_string(), PASS.to_string())])
            .run_on_socket(&listener)
            .await
            .unwrap();
    });

    port
}

async fn connect_sftp(port: u16) -> SftpSession {
    let config = Arc::new(client::Config::default());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    let mut session = loop {
        match client::connect(config.clone(), ("127.0.0.1", port), TestClient).await {
            Ok(session) => break session,
            Err(err) if tokio::time::Instant::now() < deadline => {
                let _ = err;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(err) => panic!("connect to SFTP test server: {err}"),
        }
    };

    assert!(
        session
            .authenticate_password(USER, PASS)
            .await
            .unwrap()
            .success(),
        "password auth should succeed"
    );

    let channel = session.channel_open_session().await.unwrap();
    channel.request_subsystem(true, "sftp").await.unwrap();
    SftpSession::new(channel.into_stream()).await.unwrap()
}

async fn cleanup_bucket(client: &Client, bucket: &str) {
    if let Ok(listing) = client.list_objects_v2().bucket(bucket).send().await {
        for object in listing.contents.unwrap_or_default() {
            if let Some(key) = object.key {
                let _ = client.delete_object().bucket(bucket).key(key).send().await;
            }
        }
    }

    let _ = client.delete_bucket().bucket(bucket).send().await;
}
