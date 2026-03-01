use bytes::Bytes;
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use russh_sftp::protocol::{Data, Packet, Write};
use std::hint::black_box;

fn setup_write_1kb() -> Packet {
    let data = Bytes::from(vec![0xABu8; 1024]);
    Packet::Write(Write {
        id: 1,
        handle: Bytes::from_static(b"h"),
        offset: 0,
        data,
    })
}

fn setup_write_64kb() -> Packet {
    let data = Bytes::from(vec![0xABu8; 65536]);
    Packet::Write(Write {
        id: 1,
        handle: Bytes::from_static(b"h"),
        offset: 0,
        data,
    })
}

fn setup_data_1kb() -> Packet {
    let data = Bytes::from(vec![0xABu8; 1024]);
    Packet::Data(Data { id: 1, data })
}

fn setup_data_64kb() -> Packet {
    let data = Bytes::from(vec![0xABu8; 65536]);
    Packet::Data(Data { id: 1, data })
}

fn setup_write_de_1kb() -> Bytes {
    let packet = setup_write_1kb();
    let serialized: Bytes = packet.try_into().unwrap();
    serialized.slice(4..)
}

fn setup_write_de_64kb() -> Bytes {
    let packet = setup_write_64kb();
    let serialized: Bytes = packet.try_into().unwrap();
    serialized.slice(4..)
}

fn setup_data_de_1kb() -> Bytes {
    let packet = setup_data_1kb();
    let serialized: Bytes = packet.try_into().unwrap();
    serialized.slice(4..)
}

fn setup_data_de_64kb() -> Bytes {
    let packet = setup_data_64kb();
    let serialized: Bytes = packet.try_into().unwrap();
    serialized.slice(4..)
}

#[library_benchmark]
#[bench::kb1(setup_write_1kb())]
#[bench::kb64(setup_write_64kb())]
fn bench_write_ser(packet: Packet) {
    let result: Bytes = packet.try_into().unwrap();
    black_box(result);
}

#[library_benchmark]
#[bench::kb1(setup_write_de_1kb())]
#[bench::kb64(setup_write_de_64kb())]
fn bench_write_de(mut bytes: Bytes) {
    let result: Packet = Packet::try_from(&mut bytes).unwrap();
    black_box(result);
}

#[library_benchmark]
#[bench::kb1(setup_data_1kb())]
#[bench::kb64(setup_data_64kb())]
fn bench_data_ser(packet: Packet) {
    let result: Bytes = packet.try_into().unwrap();
    black_box(result);
}

#[library_benchmark]
#[bench::kb1(setup_data_de_1kb())]
#[bench::kb64(setup_data_de_64kb())]
fn bench_data_de(mut bytes: Bytes) {
    let result: Packet = Packet::try_from(&mut bytes).unwrap();
    black_box(result);
}

library_benchmark_group!(
    name = protocol_benches;
    benchmarks = bench_write_ser, bench_write_de, bench_data_ser, bench_data_de
);

main!(library_benchmark_groups = protocol_benches);
