# AGENTS.md

## Benchmarking and profiling

Use the repo-native benchmark runner instead of ad hoc scripts:

```bash
nix develop -c cargo run --quiet --bin sftp-perf -- <subcommand> [options]
```

Supported subcommands:

- `current`
- `small-files`
- `local-stack`
- `matrix`
- `profile`
- `heaptrack`
- `list`
- `show`
- `mark-invalid`
- `mark-valid`

Common options:

- `--client bench|openssh`
- `--operation upload|download|roundtrip|all`
- `--sizes 1024,10240`
- `--ciphers aes256-gcm`
- `--note "..."` to annotate saved runs

Useful examples:

```bash
nix develop -c cargo run --quiet --bin sftp-perf -- current --client bench --sizes 1024,10240
nix develop -c cargo run --quiet --bin sftp-perf -- current --client openssh --operation all --sizes 1024
nix develop -c cargo run --quiet --bin sftp-perf -- small-files --ciphers aes256-gcm
nix develop -c cargo run --quiet --bin sftp-perf -- local-stack --russh-ref main --russh-sftp-ref master
nix develop -c cargo run --quiet --bin sftp-perf -- matrix --client bench --ciphers aes256-gcm --sizes 1024,10240
nix develop -c cargo run --quiet --bin sftp-perf -- profile --client bench --operation roundtrip --sizes 1024
nix develop -c cargo run --quiet --bin sftp-perf -- heaptrack --client openssh --operation upload --sizes 1024
nix develop -c cargo run --quiet --bin sftp-perf -- list --all
nix develop -c cargo run --quiet --bin sftp-perf -- show 1779548808-matrix
nix develop -c cargo run --quiet --bin sftp-perf -- mark-invalid 1779548493-profile --reason "system busy"
```

Compatibility wrappers still exist:

- `./benchmark-all.sh ...` -> `nix develop -c cargo run --quiet --bin sftp-perf -- matrix ...`
- `./scripts/benchmark-russh-sftp-local.sh ...` -> `nix develop -c cargo run --quiet --bin sftp-perf -- local-stack ...`

The default matrix is a 2x2 comparison:

- `current-current` = `russh main` + `russh-sftp master`
- `current-mjc` = `russh main` + `russh-sftp deserialize-bytes-optimization`
- `mjc-current` = `russh mjc/own-inbound-channel-payloads` + `russh-sftp master`
- `mjc-mjc` = `russh mjc/own-inbound-channel-payloads` + `russh-sftp deserialize-bytes-optimization`

Saved runs live under `benchmark_results/runs/<run-id>/`:

- `manifest.json` records refs, options, and machine metadata
- `results.json` stores structured timing data
- `summary.txt` is the human-readable summary
- `artifacts/` contains profiler output and bench-client JSON
- `server-*.log` contains server logs

Machine metadata now includes OS, arch, CPU model, and hostname.

The `small-files` mode is the repo-native version of the historical varied
small files benchmark. By default it transfers 1GiB as 10,251 varied-size files
with OpenSSH `sftp`, then downloads the same files back in the same batch:

```bash
nix develop -c cargo run --quiet --bin sftp-perf -- small-files
nix develop -c cargo run --quiet --bin sftp-perf -- small-files --ciphers aes256-gcm --runs 10 --warmup 2
```

### macOS specifics

- `profile` uses `xctrace` on macOS instead of Linux `perf`.
- A macOS profile run writes both `*.xctrace.trace` and `*.xctrace.xml` into `benchmark_results/runs/<run-id>/artifacts/`.
- `xctrace` must be available in `PATH`; if not, install Xcode or the Xcode command line tools.
- `local-stack`, `matrix`, and `profile` still default `--russh-repo` / `--russh-sftp-repo` to `/home/mjc/...`, so on this machine pass explicit macOS paths:

```bash
nix develop -c cargo run --quiet --bin sftp-perf -- local-stack \
  --russh-repo /Users/mjc/projects/russh \
  --russh-sftp-repo /Users/mjc/projects/russh-sftp \
  --russh-ref main \
  --russh-sftp-ref master

nix develop -c cargo run --quiet --bin sftp-perf -- profile \
  --russh-repo /Users/mjc/projects/russh \
  --russh-sftp-repo /Users/mjc/projects/russh-sftp \
  --client bench \
  --operation roundtrip \
  --sizes 1024
```

If a run was noisy or incomplete, mark it invalid so later comparisons skip it:

```bash
nix develop -c cargo run --quiet --bin sftp-perf -- mark-invalid <run-id> --reason "system busy"
```
