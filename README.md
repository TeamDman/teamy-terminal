# Teamy Terminal

Portable terminal core and Vulkan renderer for Teamy development tooling.

This repository is being bootstrapped from the
[`teamy-rust-cli`](https://github.com/TeamDman/teamy-rust-cli) template. The
template supplies the CLI conventions and quality gates; the terminal
implementation is intentionally independent of Teamy Studio, PyTorch, CUDA,
Burn, DirectX, and Minecraft.

## Current status

The workspace now contains renderer-free core, embedded Caskaydia font, a
separate portable-pty adapter, an Ash/winit Vulkan presenter, and interactive
CLI wiring. Running `cargo run` on Windows launches the default-profile
`pwsh.exe` in a Vulkan window; Vulkan construction failure is fatal and does
not select a CPU presentation fallback. The design and staged work are recorded in
[`docs/tasks/teamy terminal repository and Vulkan renderer plan.md`](docs/tasks/teamy%20terminal%20repository%20and%20Vulkan%20renderer%20plan.md).

The CLI surface already retains the useful built-ins:

```powershell
cargo run -- --help
cargo run -- --version
```

The deterministic PTY smoke uses `pwsh.exe -NoProfile` and round-trips a
bounded `1..1000` sequence. `render-fixture --format png` retains the
`rust-cpu-fontdue`/`full-png` default and produces a deterministic portable
frame artifact without requiring a window, Vulkan surface, Minecraft, or a
native GPU handle. Select the true no-window Vulkan path and any portable
pixel transport explicitly, for example:

```powershell
cargo run -p teamy-terminal-cli -- render-fixture `
  --renderer rust-gpu-slug `
  --transport dirty-raw-rgba `
  --output target\gpu-evidence\dirty.rgba `
  --manifest target\gpu-evidence\dirty.manifest.json
```

The manifest includes source/toolchain/feature/font/shader/device provenance,
native geometry, exact full-versus-dirty composition hashes, retained cache
counters, and separate geometry/upload/GPU-completion/readback/packet timings.
For a bounded
Tracy attachment workload, add named counts such as `--warmup-frames 500
--sample-frames 5000`; the default remains zero warmup frames and one measured
frame, and warmup plus samples may not exceed 10,000. Repeated payloads must
remain byte-identical to the one-shot artifact, while the manifest correlates
completed counts, frame sequences, aggregate timings, and stable Tracy stage
names. GPU manifests also aggregate every measured geometry, upload, command,
queue, completion, readback, and payload stage over exactly that sample count.

The interactive build enables the correctness-tested `dirty_rendering` path by
default, so ordinary `cargo run` redraws changed cells through the cached
frame. Use `--no-default-features` when a full-frame baseline is specifically
needed for profiling comparisons.

## Profiling

The `extended_observability` and `tracy` features are opt-in. The former owns
bounded diagnostic spans and frame statistics; the latter enables it together
with the explicitly configured Tracy transport. Human stderr, NDJSON, and
Tracy use independent filters, and profiling-only frame markers are excluded
from the human and structured logs.

Run the shared release harness with:

```powershell
./run-profiler.ps1 --log-filter debug --stop-after-duration 25s
```

The wrapper delegates to `teamy-profiler run cargo` and retains the capture,
logs, manifest, and top-span CSV under `tracy/`. `tracy-capture.exe` must be
available on `PATH`; use the profiler's `--dry-run` option to inspect target
features and artifact paths without launching a capture.

## Development

Run the repository quality gate with:

```powershell
./check-all.ps1
```

The gate preserves the template's nightly formatting, Clippy warnings-as-
errors, build, and test checks. Core, PTY, font, and renderer unit tests remain
separable from the visible-window runtime check.

The project is licensed under MPL-2.0. See the plan for the repository
bootstrap sequence, local SFM path-dependency strategy, and worktree layout.
