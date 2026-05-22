Benchmark Dependency Patches
============================

`benchmark-all.sh` applies these patches to temporary dependency checkouts before
building a matrix entry.

Patch lookup is component scoped:

- `benchmark_patches/<component>/all/*.patch`
- `benchmark_patches/<component>/<ref-name>/*.patch`
- `benchmark_patches/<component>/<matrix-label>/*.patch`

Patches must be standard `git diff`/`git format-patch` style files rooted at the
dependency repository. If a patch is already present in the resolved ref, the
script treats that as success and continues.
