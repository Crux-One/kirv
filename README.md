# KIRV: macOS CPU throttling

[![CI](https://img.shields.io/github/actions/workflow/status/Crux-One/kirv/ci.yml?logo=githubactions&style=for-the-badge)](https://github.com/Crux-One/kirv/actions/workflows/ci.yml)
[![GitHub License](https://img.shields.io/github/license/Crux-One/kirv?logo=github&style=for-the-badge)](https://github.com/Crux-One/kirv)
[![lib.rs](https://img.shields.io/badge/lib.rs-Crux--One-blue?logo=rust&style=for-the-badge)](https://lib.rs/~Crux-One)

KIRV *(/kɜːv/)* is a macOS CPU throttling utility inspired by Will Nolan's `cputhrottle` that lets you limit the CPU usage of a target process group to a requested percentage.
It brings `cputhrottle`-style CPU limiting to modern macOS systems, including Apple silicon Macs, without requiring `sudo` for processes you own.

## Usage

```bash
cargo run --release -- <pid> <percentage>
```

- `<pid>`: A process ID belonging to the process group you want to limit.
- `<percentage>`: Target `ps`/`top`-style CPU usage for the process group, from `1` to `99`.

KIRV resolves the process group for the given PID and controls all live processes in that group.

CPU percentages follow the usual `ps`/`top` convention. KIRV sums per-process CPU usage, so `100%` means one fully used logical CPU and a busy process group can exceed `100%`.
