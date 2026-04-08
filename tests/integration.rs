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

/// Start a server with a custom rekey write limit (bytes).
async fn start_test_server_with_rekey_limit(rekey_write_limit: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        Server::new(MemoryBackend::new())
            .config(
                ServerConfig::new()
                    .with_generated_key()
                    .with_rekey_write_limit(rekey_write_limit),
            )
            .with_users(vec![("test".to_string(), "pass".to_string())])
            .run_on_socket(&listener)
            .await
            .unwrap();
    });

    port
}

/// Connect with a custom client config.
/// Returns (handle, sftp_session) — keep handle alive for the duration of the session.
async fn connect_sftp_with_config(
    port: u16,
    config: Arc<client::Config>,
) -> (client::Handle<TestClient>, SftpSession) {
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
    let sftp = SftpSession::new(channel.into_stream()).await.unwrap();
    (session, sftp)
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

/// Test that an SFTP transfer survives SSH key renegotiation when the CLIENT explicitly
/// triggers rekey via `rekey_soon()` mid-transfer.
///
/// This uses a russh-to-russh connection. Both client and server handle rekey correctly
/// in this pairing (russh-to-russh rekey works). For the OpenSSH client bug, see
/// test_transfer_survives_rekey_openssh.
#[tokio::test]
async fn test_transfer_survives_rekey() {
    const UPLOAD_SIZE: usize = 3 * 1024 * 1024; // 3 MB total
    const CHUNK: usize = 512 * 1024; // write in 512KB chunks

    let port = start_test_server_with_rekey_limit(1 << 30).await;

    let (handle, sftp) = connect_sftp_with_config(port, Arc::new(client::Config::default())).await;

    let data: Vec<u8> = (0u8..=255).cycle().take(UPLOAD_SIZE).collect();

    let mut file = sftp
        .open_with_flags(
            "/rekey_test.bin",
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        )
        .await
        .unwrap();

    // Write first chunk, request rekey, then write remaining chunks.
    // rekey_soon() queues a KEXINIT to be sent at the next opportunity.
    // The server must handle all subsequent packets with the new cipher.
    let mut offset = 0;
    let mut rekeyed = false;
    while offset < data.len() {
        let end = (offset + CHUNK).min(data.len());
        file.write_all(&data[offset..end]).await.unwrap();
        offset = end;

        if !rekeyed && offset >= CHUNK {
            handle.rekey_soon().await.unwrap();
            rekeyed = true;
            // yield to let the rekey message be processed before next write
            tokio::task::yield_now().await;
        }
    }
    file.shutdown().await.unwrap();

    let mut file = sftp
        .open_with_flags("/rekey_test.bin", OpenFlags::READ)
        .await
        .unwrap();
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await.unwrap();
    assert_eq!(buf.len(), data.len(), "read back wrong number of bytes");
    assert_eq!(buf, data, "data corrupted across rekey boundary");

    drop(handle); // keep alive through the whole test
}

/// Start a server that accepts a specific public key, return (port, tempdir_with_private_key).
/// The private key is written in OpenSSH format so the system sftp client can use it.
async fn start_test_server_with_pubkey(rekey_write_limit: usize) -> (u16, tempfile::TempDir) {
    use russh::keys::ssh_key::rand_core::OsRng;
    use russh::keys::{Algorithm, PrivateKey};
    use std::io::Write as _;

    // Generate a key pair
    let privkey = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
    let pubkey = privkey.public_key().clone();

    // Write private key to a tempdir (needs 0600 permissions for sftp to accept it)
    let tmpdir = tempfile::tempdir().unwrap();
    let key_path = tmpdir.path().join("id_test");
    {
        let pem = privkey.to_openssh(Default::default()).unwrap();
        let mut f = std::fs::File::create(&key_path).unwrap();
        f.write_all(pem.as_bytes()).unwrap();
        // Set permissions to 0600 (required by OpenSSH)
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        Server::new(MemoryBackend::new())
            .config(
                ServerConfig::new()
                    .with_generated_key()
                    .with_rekey_write_limit(rekey_write_limit),
            )
            .with_pubkey_auth(move |_user, key| key == &pubkey)
            .with_users(vec![("test".to_string(), "pass".to_string())])
            .run_on_socket(&listener)
            .await
            .unwrap();
    });

    (port, tmpdir)
}

/// Test that an OpenSSH SFTP upload survives SSH rekey when `RekeyLimit=1M`
/// forces rekey during a 3MB upload.
///
/// Uses pubkey auth so sftp -b (batch mode) can authenticate non-interactively.
///
/// This is the regression scenario: affected russh revisions fail after OpenSSH
/// initiates rekey, typically with "Connection closed with error" or a timeout.
/// The test name reflects the intended behavior, so broken revisions fail here.
#[tokio::test]
async fn test_transfer_survives_rekey_openssh() {
    use std::io::Write as _;
    use tempfile::NamedTempFile;
    use tokio::process::Command;

    // Skip if sftp is unavailable
    if std::process::Command::new("sftp")
        .arg("-V")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("sftp not found, skipping test_transfer_survives_rekey_openssh");
        return;
    }

    const SERVER_REKEY_LIMIT: usize = 1 << 30;
    const UPLOAD_SIZE: usize = 3 * 1024 * 1024; // 3 MB

    let (port, _keydir) = start_test_server_with_pubkey(SERVER_REKEY_LIMIT).await;
    let key_path = _keydir.path().join("id_test");
    let key_path_str = key_path.to_str().unwrap().to_string();

    // Write the file to upload
    let mut upload_file = NamedTempFile::new().unwrap();
    let data: Vec<u8> = (0u8..=255).cycle().take(UPLOAD_SIZE).collect();
    upload_file.write_all(&data).unwrap();
    upload_file.flush().unwrap();
    let upload_path = upload_file.path().to_str().unwrap().to_string();

    // Write the sftp batch script
    let mut batch_file = NamedTempFile::new().unwrap();
    writeln!(batch_file, "put {upload_path} /uploaded.bin").unwrap();
    writeln!(batch_file, "bye").unwrap();
    batch_file.flush().unwrap();
    let batch_path = batch_file.path().to_str().unwrap().to_string();

    // Give the server task a moment to start accepting
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Run OpenSSH sftp with RekeyLimit=1M using pubkey auth (works in batch mode)
    let sftp_result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        Command::new("sftp")
            .env("SSH_AUTH_SOCK", "") // disable SSH agent
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "BatchMode=yes",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                &format!("IdentityFile={key_path_str}"),
                "-o",
                "RekeyLimit=1M",
                "-o",
                "Ciphers=aes256-gcm@openssh.com",
                "-o",
                "LogLevel=DEBUG3",
                "-P",
                &port.to_string(),
                "-b",
                &batch_path,
                "test@127.0.0.1",
            ])
            .output(),
    )
    .await;

    let output = match sftp_result {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => panic!("sftp command failed to start: {e}"),
        Err(_) => panic!(
            "sftp timed out after 30s — server stuck after rekey \
             (second post-rekey packet fails decryption)"
        ),
    };

    assert!(
        output.status.success(),
        "sftp upload with RekeyLimit=1M failed (rekey bug):\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // Verify data landed correctly via russh client connecting to same server
    let (handle, sftp) = connect_sftp_with_config(port, Arc::new(client::Config::default())).await;
    let mut file = sftp
        .open_with_flags("/uploaded.bin", OpenFlags::READ)
        .await
        .unwrap();
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await.unwrap();
    assert_eq!(buf.len(), data.len(), "uploaded file has wrong size");
    assert_eq!(buf, data, "data corrupted across rekey boundary");
    drop(handle);
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
