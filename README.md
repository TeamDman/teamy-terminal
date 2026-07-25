# Teamy Terminal

Portable terminal core and Vulkan renderer for Teamy development tooling.

This repository is being bootstrapped from the
[`teamy-rust-cli`](https://github.com/TeamDman/teamy-rust-cli) template. The
template supplies the CLI conventions and quality gates; the terminal
implementation is intentionally independent of Teamy Studio, PyTorch, CUDA,
Burn, DirectX, and Minecraft.

## Current status

The initial checkout retains the template's single-package CLI while the
workspace is being split into renderer-free core, font, Vulkan renderer, and
CLI crates. The design and staged work are recorded in
[`docs/tasks/teamy terminal repository and Vulkan renderer plan.md`](docs/tasks/teamy%20terminal%20repository%20and%20Vulkan%20renderer%20plan.md).

The CLI surface already retains the useful built-ins:

```powershell
cargo run -- --help
cargo run -- --version
```

Product subcommands will be added only after the core API is stable. The first
headless proof will run PowerShell with `-NoProfile` and emit `1..1000` into a
bounded terminal transcript; the Vulkan renderer will later produce PNG and
raw/compressed frame artifacts for consumers such as Minecraft.

## Development

Run the repository quality gate with:

```powershell
./check-all.ps1
```

The gate preserves the template's nightly formatting, Clippy warnings-as-
errors, build, and test checks. Renderer tests must remain separable from the
headless core and should not require a visible window, CUDA, or a GPU when an
off-screen/software Vulkan path is available.

The project is licensed under MPL-2.0. See the plan for the repository
bootstrap sequence, local SFM path-dependency strategy, and worktree layout.
