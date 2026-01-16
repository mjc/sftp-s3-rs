//! Simple binary for profiling large transfers with flamegraph.
//!
//! Run with: cargo flamegraph --profile profiling --bin profile_transfer

use bytes::Bytes;
use sftp_s3::backend::{Backend, MemoryBackend};
use std::time::Instant;

const KB: usize = 1024;
const MB: usize = 1024 * KB;
const GB: usize = 1024 * MB;
const CHUNK_SIZE: u32 = 64 * KB as u32;

#[tokio::main]
async fn main() {
    let size = GB; // 1GB transfer
    let chunk = Bytes::from(vec![0xABu8; CHUNK_SIZE as usize]);

    println!("Profiling 1GB roundtrip transfer...");
    println!("Chunk size: {} KB", CHUNK_SIZE / 1024);

    let start = Instant::now();

    // Perform multiple iterations for better profiling data
    for iteration in 0..5 {
        let backend = MemoryBackend::new();

        // Write (upload simulation)
        let mut write_handle = backend.open_write("test.bin").await.unwrap();
        let mut offset = 0u64;
        while offset < size as u64 {
            let write_size = std::cmp::min(CHUNK_SIZE as u64, size as u64 - offset);
            write_handle
                .write_at(offset, &chunk[..write_size as usize])
                .await
                .unwrap();
            offset += write_size;
        }
        write_handle.finish().await.unwrap();

        // Read (download simulation)
        let read_handle = backend.open_read("test.bin").await.unwrap();
        let mut offset = 0u64;
        while offset < size as u64 {
            let data = read_handle.read_at(offset, CHUNK_SIZE).await.unwrap();
            if data.is_empty() {
                break;
            }
            // Prevent optimizer from removing the read
            std::hint::black_box(&data);
            offset += data.len() as u64;
        }

        println!("Iteration {} complete", iteration + 1);
    }

    let elapsed = start.elapsed();
    let total_bytes = size * 2 * 5; // 5 iterations, read + write
    let throughput = total_bytes as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0 * 1024.0);

    println!("\nTotal time: {:.2?}", elapsed);
    println!("Total data: {} GB", total_bytes / GB);
    println!("Throughput: {:.2} GiB/s", throughput);
}
