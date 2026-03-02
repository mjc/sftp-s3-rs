//! End-to-end integration tests: real SSH/SFTP handshake against Server<MemoryBackend>

use russh::client;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use sftp_s3::{MemoryBackend, Server, ServerConfig};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Minimal russh client handler that accepts any server key (test-only)
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

/// Start a `Server<MemoryBackend>` on a random port, return the port.
/// The server runs in a background task until the test process exits.
async fn start_test_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        Server::new(MemoryBackend::new())
            .config(ServerConfig::new().with_generated_key())
            .with_users(vec![("test".to_string(), "pass".to_string())])
            .run_on_socket(&listener)
            .await
            .unwrap();
    });

    port
}

/// Connect to the test server and open an SFTP session.
async fn connect_sftp(port: u16) -> SftpSession {
    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, ("127.0.0.1", port), TestClient)
        .await
        .unwrap();

    assert!(
        session
            .authenticate_password("test", "pass")
            .await
            .unwrap()
            .success(),
        "password auth should succeed"
    );

    let channel = session.channel_open_session().await.unwrap();
    channel.request_subsystem(true, "sftp").await.unwrap();
    SftpSession::new(channel.into_stream()).await.unwrap()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_write_and_read_roundtrip() {
    let port = start_test_server().await;
    let sftp = connect_sftp(port).await;

    let mut file = sftp
        .open_with_flags(
            "/hello.txt",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .unwrap();
    file.write_all(b"hello world").await.unwrap();
    file.shutdown().await.unwrap();

    let mut file = sftp
        .open_with_flags("/hello.txt", OpenFlags::READ)
        .await
        .unwrap();
    let mut buf = String::new();
    file.read_to_string(&mut buf).await.unwrap();
    assert_eq!(buf, "hello world");
}

#[tokio::test]
async fn test_stat_on_written_file() {
    let port = start_test_server().await;
    let sftp = connect_sftp(port).await;

    let mut file = sftp
        .open_with_flags(
            "/stat_test.txt",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .unwrap();
    file.write_all(b"data").await.unwrap();
    file.shutdown().await.unwrap();

    let meta = sftp.metadata("/stat_test.txt").await.unwrap();
    assert_eq!(meta.size, Some(4));
}

#[tokio::test]
async fn test_mkdir_and_readdir() {
    let port = start_test_server().await;
    let sftp = connect_sftp(port).await;

    sftp.create_dir("/mydir").await.unwrap();

    // Write a file inside the directory
    let mut file = sftp
        .open_with_flags(
            "/mydir/file.txt",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .unwrap();
    file.write_all(b"x").await.unwrap();
    file.shutdown().await.unwrap();

    let entries: Vec<_> = sftp.read_dir("/mydir").await.unwrap().collect();
    let names: Vec<_> = entries.iter().map(|e| e.file_name()).collect();
    assert!(
        names.contains(&"file.txt".to_string()),
        "expected file.txt in /mydir, got: {names:?}"
    );
}

#[tokio::test]
async fn test_remove_file() {
    let port = start_test_server().await;
    let sftp = connect_sftp(port).await;

    let mut file = sftp
        .open_with_flags(
            "/to_delete.txt",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .unwrap();
    file.write_all(b"bye").await.unwrap();
    file.shutdown().await.unwrap();

    sftp.remove_file("/to_delete.txt").await.unwrap();

    let result = sftp.metadata("/to_delete.txt").await;
    assert!(result.is_err(), "stat after remove should fail");
}

#[tokio::test]
async fn test_rename_preserves_content() {
    let port = start_test_server().await;
    let sftp = connect_sftp(port).await;

    let mut file = sftp
        .open_with_flags(
            "/original.txt",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .unwrap();
    file.write_all(b"content").await.unwrap();
    file.shutdown().await.unwrap();

    sftp.rename("/original.txt", "/renamed.txt").await.unwrap();

    let mut file = sftp
        .open_with_flags("/renamed.txt", OpenFlags::READ)
        .await
        .unwrap();
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await.unwrap();
    assert_eq!(buf, b"content");

    let gone = sftp.metadata("/original.txt").await;
    assert!(gone.is_err(), "original path should no longer exist");
}

#[tokio::test]
async fn test_canonicalize_root() {
    let port = start_test_server().await;
    let sftp = connect_sftp(port).await;

    let canonical = sftp.canonicalize("/").await.unwrap();
    assert_eq!(canonical, "/");
}

#[tokio::test]
async fn test_large_file_roundtrip() {
    let port = start_test_server().await;
    let sftp = connect_sftp(port).await;

    // 512 KiB of content
    let data: Vec<u8> = (0u8..=255).cycle().take(512 * 1024).collect();

    let mut file = sftp
        .open_with_flags(
            "/large.bin",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .unwrap();
    file.write_all(&data).await.unwrap();
    file.shutdown().await.unwrap();

    let mut file = sftp
        .open_with_flags("/large.bin", OpenFlags::READ)
        .await
        .unwrap();
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await.unwrap();
    assert_eq!(buf, data);
}

#[tokio::test]
async fn test_wrong_password_rejected() {
    let port = start_test_server().await;

    let config = Arc::new(client::Config::default());
    let mut session = client::connect(config, ("127.0.0.1", port), TestClient)
        .await
        .unwrap();

    let result = session
        .authenticate_password("test", "wrong")
        .await
        .unwrap();
    assert!(!result.success(), "wrong password should be rejected");
}
