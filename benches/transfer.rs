use bytes::Bytes;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use sftp_s3::backend::{Backend, MemoryBackend};
use std::time::Duration;

const KB: usize = 1024;
const MB: usize = 1024 * KB;

// SFTP typically uses 32KB or 64KB chunks
const CHUNK_SIZE: u32 = 64 * KB as u32;

fn bench_streaming_write(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("streaming_write");
    group.measurement_time(Duration::from_secs(10));

    for size in [MB, 10 * MB, 100 * MB, 1024 * MB] {
        let size_name = if size >= 1024 * MB {
            format!("{}GB", size / (1024 * MB))
        } else {
            format!("{}MB", size / MB)
        };

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("memory", &size_name), &size, |b, &size| {
            // Pre-generate test data (reuse across iterations)
            let chunk = Bytes::from(vec![0xABu8; CHUNK_SIZE as usize]);

            b.to_async(&rt).iter(|| async {
                let backend = MemoryBackend::new();
                let mut handle = backend.open_write("test.bin").await.unwrap();

                let mut offset = 0u64;
                while offset < size as u64 {
                    let write_size = std::cmp::min(CHUNK_SIZE as u64, size as u64 - offset);
                    handle
                        .write_at(offset, chunk.slice(..write_size as usize))
                        .await
                        .unwrap();
                    offset += write_size;
                }

                handle.finish().await.unwrap();
            });
        });
    }

    group.finish();
}

fn bench_streaming_read(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("streaming_read");
    group.measurement_time(Duration::from_secs(10));

    for size in [MB, 10 * MB, 100 * MB, 1024 * MB] {
        let size_name = if size >= 1024 * MB {
            format!("{}GB", size / (1024 * MB))
        } else {
            format!("{}MB", size / MB)
        };

        // Pre-create the file for reading
        let backend = rt.block_on(async {
            let backend = MemoryBackend::new();
            let content = Bytes::from(vec![0xABu8; size]);
            backend.write_file("test.bin", content).await.unwrap();
            backend
        });

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("memory", &size_name), &size, |b, &size| {
            b.to_async(&rt).iter(|| async {
                let handle = backend.open_read("test.bin").await.unwrap();

                let mut offset = 0u64;
                while offset < size as u64 {
                    let data = handle.read_at(offset, CHUNK_SIZE).await.unwrap();
                    if data.is_empty() {
                        break;
                    }
                    offset += data.len() as u64;
                }
            });
        });
    }

    group.finish();
}

fn bench_roundtrip(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("roundtrip");
    group.measurement_time(Duration::from_secs(15));

    for size in [MB, 10 * MB, 100 * MB, 1024 * MB] {
        let size_name = if size >= 1024 * MB {
            format!("{}GB", size / (1024 * MB))
        } else {
            format!("{}MB", size / MB)
        };

        // Measure write + read (upload then download)
        group.throughput(Throughput::Bytes(size as u64 * 2)); // Both directions
        group.bench_with_input(BenchmarkId::new("memory", &size_name), &size, |b, &size| {
            let chunk = Bytes::from(vec![0xABu8; CHUNK_SIZE as usize]);

            b.to_async(&rt).iter(|| async {
                let backend = MemoryBackend::new();

                // Write (upload)
                let mut write_handle = backend.open_write("test.bin").await.unwrap();
                let mut offset = 0u64;
                while offset < size as u64 {
                    let write_size = std::cmp::min(CHUNK_SIZE as u64, size as u64 - offset);
                    write_handle
                        .write_at(offset, chunk.slice(..write_size as usize))
                        .await
                        .unwrap();
                    offset += write_size;
                }
                write_handle.finish().await.unwrap();

                // Read (download)
                let read_handle = backend.open_read("test.bin").await.unwrap();
                let mut offset = 0u64;
                while offset < size as u64 {
                    let data = read_handle.read_at(offset, CHUNK_SIZE).await.unwrap();
                    if data.is_empty() {
                        break;
                    }
                    offset += data.len() as u64;
                }
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_streaming_write,
    bench_streaming_read,
    bench_roundtrip
);
criterion_main!(benches);
