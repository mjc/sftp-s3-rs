# AGENTS.md

## Benchmarking and profiling

Use the repo-native benchmark runner instead of ad hoc scripts:

```bash
nix develop -c ./perf.sh <subcommand> [options]
```

Supported subcommands:

- `current`
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
nix develop -c ./perf.sh current --client bench --sizes 1024,10240
nix develop -c ./perf.sh current --client openssh --operation all --sizes 1024
nix develop -c ./perf.sh local-stack --russh-ref main --russh-sftp-ref master
nix develop -c ./perf.sh matrix --client bench --ciphers aes256-gcm --sizes 1024,10240
nix develop -c ./perf.sh profile --client bench --operation roundtrip --sizes 1024
nix develop -c ./perf.sh heaptrack --client openssh --operation upload --sizes 1024
nix develop -c ./perf.sh list --all
nix develop -c ./perf.sh show 1779548808-matrix
nix develop -c ./perf.sh mark-invalid 1779548493-profile --reason "system busy"
```

Compatibility wrappers still exist:

- `./benchmark-all.sh ...` -> `./perf.sh matrix ...`
- `./scripts/benchmark-russh-sftp-local.sh ...` -> `./perf.sh local-stack ...`

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

If a run was noisy or incomplete, mark it invalid so later comparisons skip it:

```bash
nix develop -c ./perf.sh mark-invalid <run-id> --reason "system busy"
```

