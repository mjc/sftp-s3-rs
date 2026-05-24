use object::{Object, ObjectSymbol, SymbolKind};
use rustc_demangle::try_demangle;
use rustc_hash::FxHashMap as HashMap;
use std::env;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

type BoxError = Box<dyn std::error::Error>;
type FrameId = u32;

const PERF_MAGIC: &[u8; 8] = b"PERFILE2";

const PERF_RECORD_MMAP: u32 = 1;
const PERF_RECORD_COMM: u32 = 3;
const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_RECORD_MMAP2: u32 = 10;

const PERF_SAMPLE_IP: u64 = 1 << 0;
const PERF_SAMPLE_TID: u64 = 1 << 1;
const PERF_SAMPLE_TIME: u64 = 1 << 2;
const PERF_SAMPLE_CALLCHAIN: u64 = 1 << 5;
const PERF_SAMPLE_PERIOD: u64 = 1 << 8;
const PERF_SAMPLE_ADDR: u64 = 1 << 3;
const PERF_SAMPLE_READ: u64 = 1 << 4;
const PERF_SAMPLE_ID: u64 = 1 << 6;
const PERF_SAMPLE_STREAM_ID: u64 = 1 << 9;
const PERF_SAMPLE_CPU: u64 = 1 << 7;
const PERF_SAMPLE_IDENTIFIER: u64 = 1 << 16;

const SUPPORTED_SAMPLE_TYPE: u64 = PERF_SAMPLE_IP
    | PERF_SAMPLE_TID
    | PERF_SAMPLE_TIME
    | PERF_SAMPLE_CALLCHAIN
    | PERF_SAMPLE_PERIOD;

const PERF_CONTEXT_MAX: u64 = u64::MAX - 4095;

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), BoxError> {
    let config = Config::parse()?;
    if config.help {
        print_usage(&config.program);
        return Ok(());
    }

    let bytes = fs::read(&config.input)
        .map_err(|err| format!("failed to read {}: {err}", config.input.display()))?;
    let mut perf = PerfData::parse(&bytes)?;
    let mut symbols = SymbolCache::new(
        config.show_offsets,
        config.count_periods,
        config.collapse_kernel,
        config.sort,
    );
    let stacks = perf.fold(&mut symbols)?;

    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(1024 * 1024, stdout.lock());
    for (stack, count) in stacks {
        writeln!(out, "{stack} {count}")?;
    }
    Ok(())
}

struct Config {
    program: String,
    input: PathBuf,
    show_offsets: bool,
    count_periods: bool,
    collapse_kernel: bool,
    sort: bool,
    help: bool,
}

impl Config {
    fn parse() -> Result<Self, BoxError> {
        let mut args = env::args();
        let program = args.next().unwrap_or_else(|| "perf-fold".to_string());
        let mut input = None;
        let mut show_offsets = false;
        let mut count_periods = false;
        let mut collapse_kernel = false;
        let mut sort = false;

        for arg in args {
            match arg.as_str() {
                "-h" | "--help" => {
                    return Ok(Self {
                        program,
                        input: PathBuf::new(),
                        show_offsets,
                        count_periods,
                        collapse_kernel,
                        sort,
                        help: true,
                    });
                }
                "--show-offsets" => show_offsets = true,
                "--count-periods" => count_periods = true,
                "--collapse-kernel" => collapse_kernel = true,
                "--sort" => sort = true,
                _ if arg.starts_with('-') => return Err(format!("unknown option: {arg}").into()),
                _ => {
                    if input.replace(PathBuf::from(arg)).is_some() {
                        return Err("only one perf.data path may be provided".into());
                    }
                }
            }
        }

        let input = input.ok_or_else(|| {
            print_usage(&program);
            "missing perf.data path"
        })?;

        Ok(Self {
            program,
            input,
            show_offsets,
            count_periods,
            collapse_kernel,
            sort,
            help: false,
        })
    }
}

fn print_usage(program: &str) {
    eprintln!(
        "Usage: {program} [--show-offsets] [--count-periods] [--collapse-kernel] [--sort] <perf.data>"
    );
    eprintln!();
    eprintln!("Writes Inferno folded stacks to stdout:");
    eprintln!("  {program} perf.data | inferno-flamegraph > flamegraph.svg");
    eprintln!();
    eprintln!("This fast path supports perf.data files recorded with:");
    eprintln!("  perf record -g --call-graph fp ...");
    eprintln!(
        "By default each sample counts as 1. Use --count-periods to weight by sample period."
    );
    eprintln!("Kernel frames are preserved by default. Use --collapse-kernel to group them.");
    eprintln!("Folded stacks are unsorted by default. Use --sort for deterministic output.");
}

struct PerfData<'a> {
    bytes: &'a [u8],
    data_offset: usize,
    data_size: usize,
    sample_type: u64,
    sample_id_all: bool,
    mmaps: MmapTable,
    comms: HashMap<u32, String>,
}

impl<'a> PerfData<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, BoxError> {
        if bytes.len() < 104 || &bytes[..8] != PERF_MAGIC {
            return Err("not a supported perf.data file".into());
        }

        let header_size = read_u64(bytes, 8)? as usize;
        let attr_size = read_u64(bytes, 16)? as usize;
        if header_size < 104 || bytes.len() < header_size {
            return Err("invalid perf.data header".into());
        }

        let attrs = Section {
            offset: read_u64(bytes, 24)? as usize,
            size: read_u64(bytes, 32)? as usize,
        };
        let data = Section {
            offset: read_u64(bytes, 40)? as usize,
            size: read_u64(bytes, 48)? as usize,
        };

        if attrs.size < attr_size || attrs.offset + attrs.size > bytes.len() {
            return Err("invalid perf attr section".into());
        }
        if data.offset + data.size > bytes.len() {
            return Err("invalid perf data section".into());
        }

        let attr = &bytes[attrs.offset..attrs.offset + attr_size];
        let sample_type = read_u64(attr, 24)?;
        if sample_type & SUPPORTED_SAMPLE_TYPE != SUPPORTED_SAMPLE_TYPE {
            return Err(format!(
                "unsupported sample_type 0x{sample_type:x}; need IP|TID|TIME|CALLCHAIN|PERIOD"
            )
            .into());
        }

        let flags = read_u64(attr, 40)?;
        let sample_id_all = flags & (1 << 21) != 0;

        Ok(Self {
            bytes,
            data_offset: data.offset,
            data_size: data.size,
            sample_type,
            sample_id_all,
            mmaps: MmapTable::default(),
            comms: HashMap::default(),
        })
    }

    fn fold(&mut self, symbols: &mut SymbolCache) -> Result<Vec<(String, u64)>, BoxError> {
        let mut interner = FrameInterner::default();
        let mut counts: HashMap<Vec<FrameId>, u64> = HashMap::default();
        let mut offset = self.data_offset;
        let end = self.data_offset + self.data_size;

        while offset + 8 <= end {
            let record_type = read_u32(self.bytes, offset)?;
            let size = read_u16(self.bytes, offset + 6)? as usize;
            if size < 8 || offset + size > end {
                return Err(format!("invalid perf record at offset {offset}").into());
            }

            let body_start = offset + 8;
            let body_end = offset + size;
            let body = &self.bytes[body_start..body_end];

            match record_type {
                PERF_RECORD_MMAP => self.read_mmap(body, false)?,
                PERF_RECORD_MMAP2 => self.read_mmap(body, true)?,
                PERF_RECORD_COMM => self.read_comm(body)?,
                PERF_RECORD_SAMPLE => {
                    if let Some((pid, stack, period)) = self
                        .read_sample(body)
                        .map_err(|err| format!("failed to read sample at offset {offset}: {err}"))?
                    {
                        let mut frames = Vec::with_capacity(stack.len() + 1);
                        if let Some(comm) = self.comms.get(&pid) {
                            frames.push(interner.intern_name(clean_frame(comm)));
                        }

                        for ip in stack.into_iter().rev() {
                            if ip >= PERF_CONTEXT_MAX {
                                continue;
                            }
                            frames.push(interner.intern_ip(pid, ip, symbols, &self.mmaps));
                        }

                        if !frames.is_empty() {
                            let count = if symbols.count_periods {
                                period.max(1)
                            } else {
                                1
                            };
                            *counts.entry(frames).or_insert(0) += count;
                        }
                    }
                }
                _ => {}
            }

            offset += size;
        }

        let mut stacks: Vec<_> = counts
            .into_iter()
            .map(|(stack, count)| (interner.render_stack(&stack), count))
            .collect();
        if symbols.sort {
            stacks.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        }
        Ok(stacks)
    }

    fn read_mmap(&mut self, body: &[u8], mmap2: bool) -> Result<(), BoxError> {
        let fixed = if mmap2 { 64 } else { 32 };
        if body.len() < fixed {
            return Ok(());
        }

        let pid = read_u32(body, 0)?;
        let addr = read_u64(body, 8)?;
        let len = read_u64(body, 16)?;
        let pgoff = read_u64(body, 24)?;

        let filename_end = body[fixed..]
            .iter()
            .position(|b| *b == 0)
            .map(|idx| fixed + idx)
            .unwrap_or(body.len());
        let filename = String::from_utf8_lossy(&body[fixed..filename_end]).into_owned();
        if filename.is_empty() || filename.starts_with('[') || len == 0 {
            return Ok(());
        }

        self.mmaps.add(Mapping {
            pid,
            start: addr,
            end: addr.saturating_add(len),
            pgoff,
            path: PathBuf::from(filename),
        });
        Ok(())
    }

    fn read_comm(&mut self, body: &[u8]) -> Result<(), BoxError> {
        if body.len() < 8 {
            return Ok(());
        }
        let pid = read_u32(body, 0)?;
        let comm_end = body[8..]
            .iter()
            .position(|b| *b == 0)
            .map(|idx| 8 + idx)
            .unwrap_or(body.len());
        let comm = String::from_utf8_lossy(&body[8..comm_end]).into_owned();
        if !comm.is_empty() {
            self.comms.insert(pid, comm);
        }
        Ok(())
    }

    fn read_sample(&self, body: &[u8]) -> Result<Option<(u32, Vec<u64>, u64)>, BoxError> {
        let mut cursor = Cursor::new(body);

        if self.sample_type & PERF_SAMPLE_IDENTIFIER != 0 {
            cursor.skip(8)?;
        }

        let ip = if self.sample_type & PERF_SAMPLE_IP != 0 {
            cursor.u64()?
        } else {
            0
        };

        let pid = if self.sample_type & PERF_SAMPLE_TID != 0 {
            let pid = cursor.u32()?;
            let _tid = cursor.u32()?;
            pid
        } else {
            0
        };

        if self.sample_type & PERF_SAMPLE_TIME != 0 {
            cursor.skip(8)?;
        }
        if self.sample_type & PERF_SAMPLE_ADDR != 0 {
            cursor.skip(8)?;
        }
        if self.sample_type & PERF_SAMPLE_ID != 0 {
            cursor.skip(8)?;
        }
        if self.sample_type & PERF_SAMPLE_STREAM_ID != 0 {
            cursor.skip(8)?;
        }
        if self.sample_type & PERF_SAMPLE_CPU != 0 {
            cursor.skip(8)?;
        }

        let period = if self.sample_type & PERF_SAMPLE_PERIOD != 0 {
            cursor.u64()?
        } else {
            1
        };

        if self.sample_type & PERF_SAMPLE_READ != 0 {
            return Err("PERF_SAMPLE_READ is not supported yet".into());
        }

        let mut stack = Vec::new();
        if ip != 0 {
            stack.push(ip);
        }

        if self.sample_type & PERF_SAMPLE_CALLCHAIN != 0 {
            let nr = cursor.u64()? as usize;
            stack.reserve(nr);
            for _ in 0..nr {
                stack.push(cursor.u64()?);
            }
        }

        if self.sample_id_all {
            let _ = cursor.remaining();
        }

        if stack.is_empty() {
            return Ok(None);
        }
        Ok(Some((pid, stack, period)))
    }
}

#[derive(Clone, Copy)]
struct Section {
    offset: usize,
    size: usize,
}

#[derive(Clone)]
struct Mapping {
    pid: u32,
    start: u64,
    end: u64,
    pgoff: u64,
    path: PathBuf,
}

#[derive(Default)]
struct MmapTable {
    by_pid: HashMap<u32, Vec<Mapping>>,
    all: Vec<Mapping>,
}

impl MmapTable {
    fn add(&mut self, mapping: Mapping) {
        self.by_pid
            .entry(mapping.pid)
            .or_default()
            .push(mapping.clone());
        self.all.push(mapping);
    }

    fn find(&self, pid: u32, ip: u64) -> Option<&Mapping> {
        self.by_pid
            .get(&pid)
            .and_then(|maps| find_mapping(maps, ip))
            .or_else(|| find_mapping(&self.all, ip))
    }
}

fn find_mapping(maps: &[Mapping], ip: u64) -> Option<&Mapping> {
    maps.iter()
        .rev()
        .find(|mapping| ip >= mapping.start && ip < mapping.end)
}

#[derive(Default)]
struct FrameInterner {
    names: Vec<String>,
    by_name: HashMap<String, FrameId>,
    by_ip: HashMap<(u32, u64), FrameId>,
}

impl FrameInterner {
    fn intern_name(&mut self, name: String) -> FrameId {
        if let Some(id) = self.by_name.get(&name) {
            return *id;
        }
        let id = self.names.len() as FrameId;
        self.names.push(name.clone());
        self.by_name.insert(name, id);
        id
    }

    fn intern_ip(
        &mut self,
        pid: u32,
        ip: u64,
        symbols: &mut SymbolCache,
        mmaps: &MmapTable,
    ) -> FrameId {
        let key = (pid, ip);
        if let Some(id) = self.by_ip.get(&key) {
            return *id;
        }
        let id = self.intern_name(symbols.resolve(ip, pid, mmaps));
        self.by_ip.insert(key, id);
        id
    }

    fn render_stack(&self, stack: &[FrameId]) -> String {
        let mut len = stack.len().saturating_sub(1);
        for frame in stack {
            len += self.names[*frame as usize].len();
        }

        let mut rendered = String::with_capacity(len);
        for (idx, frame) in stack.iter().enumerate() {
            if idx > 0 {
                rendered.push(';');
            }
            rendered.push_str(&self.names[*frame as usize]);
        }
        rendered
    }
}

struct SymbolCache {
    files: HashMap<PathBuf, SymbolFile>,
    show_offsets: bool,
    count_periods: bool,
    collapse_kernel: bool,
    sort: bool,
}

impl SymbolCache {
    fn new(show_offsets: bool, count_periods: bool, collapse_kernel: bool, sort: bool) -> Self {
        Self {
            files: HashMap::default(),
            show_offsets,
            count_periods,
            collapse_kernel,
            sort,
        }
    }

    fn resolve(&mut self, ip: u64, pid: u32, mmaps: &MmapTable) -> String {
        if self.collapse_kernel && is_kernel_ip(ip) {
            return "[kernel]".to_string();
        }

        let Some(mapping) = mmaps.find(pid, ip) else {
            return format!("0x{ip:x}");
        };

        let rel = ip
            .saturating_sub(mapping.start)
            .saturating_add(mapping.pgoff);
        let file = self
            .files
            .entry(mapping.path.clone())
            .or_insert_with(|| SymbolFile::load(&mapping.path));

        match file.lookup(rel).or_else(|| file.lookup(ip)) {
            Some(symbol) if self.show_offsets => {
                let offset = rel.saturating_sub(symbol.addr);
                format!("{}+0x{offset:x}", symbol.name)
            }
            Some(symbol) => symbol.name.clone(),
            None => format!("{}:0x{rel:x}", mapping.path.display()),
        }
    }
}

fn is_kernel_ip(ip: u64) -> bool {
    ip & 0xffff_0000_0000_0000 == 0xffff_0000_0000_0000
}

struct SymbolFile {
    symbols: Vec<Symbol>,
}

impl SymbolFile {
    fn load(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self {
                symbols: Vec::new(),
            };
        };
        let Ok(object) = object::File::parse(bytes.as_slice()) else {
            return Self {
                symbols: Vec::new(),
            };
        };

        let mut symbols = Vec::new();
        for symbol in object.symbols().chain(object.dynamic_symbols()) {
            if symbol.kind() != SymbolKind::Text || symbol.address() == 0 {
                continue;
            }
            let Ok(name) = symbol.name() else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            symbols.push(Symbol {
                addr: symbol.address(),
                size: symbol.size(),
                name: clean_frame(&demangle(name)),
            });
        }

        symbols.sort_by(|a, b| {
            a.addr
                .cmp(&b.addr)
                .then_with(|| b.size.cmp(&a.size))
                .then_with(|| a.name.cmp(&b.name))
        });
        symbols.dedup_by(|a, b| a.addr == b.addr && a.name == b.name);
        Self { symbols }
    }

    fn lookup(&self, addr: u64) -> Option<&Symbol> {
        let idx = self
            .symbols
            .partition_point(|symbol| symbol.addr <= addr)
            .checked_sub(1)?;
        let symbol = &self.symbols[idx];
        if symbol.size == 0 || addr < symbol.addr.saturating_add(symbol.size) {
            Some(symbol)
        } else {
            None
        }
    }
}

struct Symbol {
    addr: u64,
    size: u64,
    name: String,
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn skip(&mut self, len: usize) -> Result<(), BoxError> {
        if self.offset + len > self.bytes.len() {
            return Err("truncated sample record".into());
        }
        self.offset += len;
        Ok(())
    }

    fn u32(&mut self) -> Result<u32, BoxError> {
        let value = read_u32(self.bytes, self.offset)?;
        self.offset += 4;
        Ok(value)
    }

    fn u64(&mut self) -> Result<u64, BoxError> {
        let value = read_u64(self.bytes, self.offset)?;
        self.offset += 8;
        Ok(value)
    }
}

fn demangle(name: &str) -> String {
    try_demangle(name)
        .map(|name| name.to_string())
        .unwrap_or_else(|_| name.to_string())
}

fn clean_frame(name: &str) -> String {
    name.replace(';', "\\;").replace('\n', " ")
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, BoxError> {
    let data = bytes
        .get(offset..offset + 2)
        .ok_or("unexpected end of perf.data")?;
    Ok(u16::from_le_bytes(data.try_into()?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, BoxError> {
    let data = bytes
        .get(offset..offset + 4)
        .ok_or("unexpected end of perf.data")?;
    Ok(u32::from_le_bytes(data.try_into()?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, BoxError> {
    let data = bytes
        .get(offset..offset + 8)
        .ok_or("unexpected end of perf.data")?;
    Ok(u64::from_le_bytes(data.try_into()?))
}
