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
bounded `1..1000` sequence. Portable frame encoding, off-screen readback, and
SFM integration remain outside the standalone interactive MVP.

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
