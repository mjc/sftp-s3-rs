use bytes::Bytes;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use russh_sftp::protocol::{Data, DataPayload, Packet, Write};
use std::hint::black_box;

const KB: usize = 1024;

fn bench_write_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_serialize");
    group.measurement_time(std::time::Duration::from_secs(10));
    group.sample_size(100);

    for size in [KB, 4 * KB, 16 * KB, 64 * KB, 256 * KB] {
        let size_name = format!("{}KB", size / KB);
        let data = black_box(Bytes::from(vec![0xABu8; size]));

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("ser", &size_name), &data, |b, data| {
            b.iter(|| {
                let packet = Packet::Write(Write {
                    id: 1,
                    handle: Bytes::from_static(b"h"),
                    offset: 0,
                    data: data.clone(),
                });
                let _: Bytes = packet.try_into().unwrap();
            });
        });
    }
    group.finish();
}

fn bench_write_deserialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_deserialize");
    group.measurement_time(std::time::Duration::from_secs(10));
    group.sample_size(100);

    for size in [KB, 4 * KB, 16 * KB, 64 * KB, 256 * KB] {
        let size_name = format!("{}KB", size / KB);
        let data = Bytes::from(vec![0xABu8; size]);

        let packet = Packet::Write(Write {
            id: 1,
            handle: Bytes::from_static(b"h"),
            offset: 0,
            data: data.clone(),
        });
        let serialized: Bytes = packet.try_into().unwrap();
        let serialized = black_box(serialized);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("de", &size_name),
            &serialized,
            |b, serialized| {
                b.iter(|| {
                    let mut bytes = serialized.slice(4..);
                    let _: Packet = Packet::try_from(&mut bytes).unwrap();
                });
            },
        );
    }
    group.finish();
}

fn bench_data_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("data_serialize");
    group.measurement_time(std::time::Duration::from_secs(10));
    group.sample_size(100);

    for size in [KB, 4 * KB, 16 * KB, 64 * KB, 256 * KB] {
        let size_name = format!("{}KB", size / KB);
        let data = black_box(Bytes::from(vec![0xABu8; size]));

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("ser", &size_name), &data, |b, data| {
            b.iter(|| {
                let packet = Packet::Data(Data {
                    id: 1,
                    data: DataPayload::Bytes(data.clone()),
                });
                let _: Bytes = packet.try_into().unwrap();
            });
        });
    }
    group.finish();
}

fn bench_data_deserialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("data_deserialize");
    group.measurement_time(std::time::Duration::from_secs(10));
    group.sample_size(100);

    for size in [KB, 4 * KB, 16 * KB, 64 * KB, 256 * KB] {
        let size_name = format!("{}KB", size / KB);
        let data = Bytes::from(vec![0xABu8; size]);

        let packet = Packet::Data(Data {
            id: 1,
            data: DataPayload::Bytes(data.clone()),
        });
        let serialized: Bytes = packet.try_into().unwrap();
        let serialized = black_box(serialized);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("de", &size_name),
            &serialized,
            |b, serialized| {
                b.iter(|| {
                    let mut bytes = serialized.slice(4..);
                    let _: Packet = Packet::try_from(&mut bytes).unwrap();
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_write_serialize,
    bench_write_deserialize,
    bench_data_serialize,
    bench_data_deserialize,
);
criterion_main!(benches);
