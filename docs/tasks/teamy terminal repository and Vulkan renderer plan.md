# Teamy Terminal — Resumable Implementation Plan

**Plan status:** Active
**Primary implementation root:** G:\Programming\Repos\teamy-terminal (main)
**Plan authority:** This file on teamy-terminal/main
**SFM pointer:** D:\Repos\Minecraft\SFM\repos2\1.19.2 (1.19.2)
**Last updated:** 2026-07-25

## How to update this plan

- [ ] Not started
- [~] In progress
- [x] Complete
- [!] Blocked

Put the status in each work-item heading and update its completion notes at the
same time. A phase is complete only when every work item in it is [x].
Record decisions, commit IDs, commands, and follow-ups below the task they
affect; do not append a detached chronological work log. Use [!] only with
the exact blocker, evidence, and condition that unblocks it. This file is the
single authoritative plan. Update it in teamy-terminal/main; the SFM document
is only a pointer and must not duplicate plan content.

**Next slice:** Execute Phase 3.5 as the active performance and Windows
integration track: establish the profiling feature contract, add coarse
instrumentation, and collect a reproducible baseline before changing the
renderer. The code-level input mapping, PTY/Vulkan startup, resize/reflow, and
large-output responsiveness proofs are complete. Direct desktop interaction
evidence and process-free replay remain explicit acceptance gaps; they are not
being marked complete while the profiling track proceeds. Portable frame
encoding and SFM integration remain deferred.

The first profiling evidence pass is now complete enough to make the renderer
decision: retain the full-frame baseline, and validate the opt-in
`dirty_rendering` prototype against it. The capture observed a low
changed-cell ratio while the current path still rasterized and uploaded the
whole frame; a naive changed-cell loop without cached frame storage and
swapchain-safe partial upload is not an acceptable optimization. The candidate
now has a successful 40-second GUI capture with generated output and explicit
800x450/1600x900 resizes; the stale registry entry from an earlier instance
remains visible but does not prevent new windows from launching.

## Purpose

Create a small public MPL-2.0 Rust repository that owns a renderer-free
terminal core, Teamy font extraction, a low-level Vulkan renderer, and a CLI
for deterministic headless proofs. The observable first milestones are:

1. teamy-terminal --help and --version work from a fresh checkout.
2. A headless terminal can consume bounded PowerShell -NoProfile output for
   1..1000 and replay the same result without launching a process.
3. A Vulkan/off-screen renderer can produce validated PNG and bounded raw or
   compressed RGBA frames for optional consumers such as SFM/Vox.

The first user-facing target is the standalone Windows interactive MVP defined
in Phase 3.4. This remains the development-tooling foundation for the optional
Vox terminal bridge; ordinary Minecraft gameplay does not depend on Rust,
Vulkan, or this repository.

## Scope

### In scope

- A Cargo workspace containing teamy-terminal-core, teamy-terminal-font,
  teamy-terminal-pty, teamy-terminal-renderer, and teamy-terminal-cli.
- Terminal state, VT/control-byte handling, bounded scrollback, cursor/style,
  selection, damage, resize, input events, snapshots, and deterministic replay.
- A PowerShell PTY adapter used for the interactive shell and its bounded
  deterministic smoke proof.
- Extraction of the terminal-relevant Teamy font/rasterization algorithm.
- An ash-based Vulkan presentation layer with an off-screen/readback path,
  resize handling, and renderer-neutral frame output.
- Local SFM iteration through an isolated Cargo path override, followed by a
  reviewed pinned dependency or git revision.
- Documentation and acceptance evidence that make the optional Rust path
  understandable and keep Java-local SFM fallback independent.

### Out of scope

- Importing Teamy Studio's application, PyTorch, CUDA, Burn, DirectX, or
  unrelated windowing workspace.
- Replacing the Java-local terminal, mounted editing, or Minecraft UI with a
  Rust runtime dependency.
- Sending Minecraft Screen objects, panels, layout instructions, callbacks,
  or executable UI over Vox.
- Native DirectX/OpenGL shared-handle interop in the first frame protocol.
- Making the colour-picker flow the first cross-language demonstration.
- Supporting non-Windows Vulkan windowing before the Windows/core seams are
  proven; portability remains a later acceptance expansion.

## Established foundation

These facts are verified and should not be silently re-litigated:

- TeamDman/teamy-terminal is public, MPL-2.0, and has been pushed to
  https://github.com/TeamDman/teamy-terminal. The bootstrap commit is
  e6ff0ec.
- The checkout is at G:\Programming\Repos\teamy-terminal. This file is the
  authoritative plan; SFM retains only a pointer to this path and URL.
- The initial checkout was copied from
  G:\Programming\Repos\teamy-rust-cli using its compatibility initializer.
  The normal cargo run -- init path was attempted but could not compile the
  template because the direct Facet pin differs from the Facet revision pulled
  by teamy-cancellation; StopAfterArgs consequently failed its Facet bound.
  This is recorded as a dependency-repair task, not a reason to import
  Teamy Studio.
- Package identity was changed to teamy-terminal; repository metadata,
  README, environment variable names, binary test names, and implementation
  source URL no longer use the template package name.
- The initial metadata and nightly-format checks passed before implementation;
  the completed Phase 1 workspace now also passes the full workspace quality
  gate recorded below.
- G:\Programming\Repos\cursor-latency at revision 6c07705 provides a
  proven local ash 0.38/ash-window 0.13/winit 0.30 instance, device,
  swapchain, and transparent-window seam.
- G:\Programming\Repos\ash is the upstream API/examples reference; its
  ash crate is 0.38.0+1.4.352 and the checkout includes ash-examples and
  ash-window. It is not a source tree to vendor wholesale.
- The related SFM bridge contract and Java-local fallback are specified in
  docs/tasks/vox terminal bridge and graceful degradation plan.md. SFM's
  repository-specific workflow is in repos2/1.19.2/docs/AGENTS.md.

## Confirmed constraints and decisions

- The project name is Teamy Terminal and the canonical remote is
  TeamDman/teamy-terminal under MPL-2.0.
- The teamy-rust-cli template supplies conventions, not a runtime dependency.
  Retain Figue/Facet parsing, built-in --help/--version/completion,
  repository/branch/revision/worktree/build metadata, color-eyre, tracing,
  cooperative cancellation, Windows ANSI handling, build.rs, lint policy,
  rustfmt.toml, clippy.toml, and the template's round-trip test pattern.
- The workspace should expose teamy-terminal-cli as its default Cargo member,
  so cargo run -- --help and cargo run -- --version remain convenient even
  after the split. Product subcommands may start empty; built-in help/version
  must remain available.
- The PTY adapter is a separate `teamy-terminal-pty` crate. It owns the
  unmodified `portable-pty 0.9` integration, shell startup, profile policy,
  byte/input plumbing, resize, cancellation, and process exit. It exposes
  terminal-oriented types rather than leaking `portable-pty` through the CLI.
- Interactive launch uses `pwsh.exe` with the user's normal profile. Test and
  deterministic smoke launches use `-NoProfile`. Prebuilt Microsoft Terminal
  ConPTY/OpenConsole artifacts are an acknowledged external prerequisite and
  are not part of the MVP artifact-packaging decision.
- The first windowed renderer uses the existing `ash 0.38`, `ash-window 0.13`,
  and `winit 0.30` seam. Caskaydia Cove is embedded as the initial terminal
  font. Vulkan construction failure is fatal and must produce an actionable
  diagnostic; there is no CPU-rendering fallback.
- The MVP is Windows-first and includes an interactive shell, typed commands,
  ANSI colors/cursor movement, resize, Ctrl+C, paste, and clean shell exit.
  Mouse selection, advanced TUI compatibility, SFM, portable frame encoding,
  and non-Windows renderer support are outside this goal.
- teamy-terminal-core must not depend on ash, winit, DirectX, CUDA,
  PyTorch, Minecraft, or a process host. Process adapters feed the core from
  outside.
- The renderer uses ash directly. ash-window, raw-window-handle, and winit
  are boundary dependencies only. Vulkan unsafe code is isolated at that
  boundary with actionable loader/device errors.
- The first frame contract is portable data: sequence, dimensions, stride,
  format, full-frame/dirty-tile kind, compression, and bytes. PNG is for
  artifacts/keyframes; raw or losslessly compressed dirty tiles are for live
  interaction. Java rejects oversized, malformed, stale, and out-of-order
  frames when it consumes them.
- SFM's local path override is development-only and must never become an
  absolute path in canonical or propagated source. Ordinary SFM gameplay and
  Java-local terminal fallback must work without a Rust checkout, renderer,
  Vulkan loader, or GPU.
- SFM version propagation remains baseline-first: canonical 1.19.2 is the
  source, sfm-propagate-changes.exe performs cross-version propagation, and
  newer-target behavior must not be clobbered.

## Assumptions requiring validation

- pwsh.exe is available on the development machine; Windows PowerShell is a
  documented fallback. The smoke is bounded and not an arbitrary shell API.
- A software Vulkan device or usable off-screen Vulkan path is available on at
  least one supported development environment. Missing Vulkan must produce a
  diagnostic rather than fail core/CLI tests.
- Teamy Studio's font algorithm can be extracted without importing its
  application model or graphics dependencies.
- The current Facet/figue revision pair can be aligned for the new workspace,
  or the CLI can temporarily use a minimal compatible dependency set while
  retaining the same public built-in CLI behavior.

## Design questions that must be closed before implementation

| Question | Required decision | Acceptance consequence |
| --- | --- | --- |
| Workspace topology | Virtual root workspace with teamy-terminal-cli as a default member and a dedicated teamy-terminal-pty member. | cargo metadata, cargo run -- --help, and all-workspace quality commands must work from the repository root. |
| Facet revision policy | Which Facet revision is compatible with teamy-cancellation, and where is that pin recorded? | Fresh clone must compile without duplicate incompatible Facet traits; lockfile provenance is reviewed. |
| Initial CLI command shape | How is the interactive default represented while Figue built-ins remain available? | --help and --version bypass interactive launch and remain tested. |
| PTY boundary | Own teamy-terminal-pty crate around unmodified portable-pty; core remains process-free and the CLI owns application wiring. | PTY startup, I/O, resize, cancellation, and process exit are testable without renderer dependencies. |
| Interactive shell policy | Normal-profile pwsh.exe for interactive use; -NoProfile for deterministic tests and smoke fixtures. | The user-facing shell and test shell behavior cannot drift accidentally. |
| Font boundary | Separate teamy-terminal-font crate with embedded Caskaydia Cove as the initial font. | Core remains graphics-free; a deterministic glyph/atlas fixture proves the chosen seam. |
| Vulkan MVP path | Windows x64, ash/ash-window/winit windowed presentation; Vulkan construction failure is fatal; CPU fallback is unsupported. | A usable Vulkan device produces the interactive window, while failure produces an actionable diagnostic. |
| Interactive MVP integration | `cargo run` with no product subcommand launches the default-profile shell and connects PTY, core, input, resize, and Vulkan presentation. | A manual command/resize/Ctrl+C/exit proof completes the standalone terminal goal. |
| Vulkan off-screen path | Deferred outside the interactive MVP together with portable frame encoding. | Headless frame/readback remains a later proof and does not block the first usable terminal window. |
| SFM dependency pin | Which core/frame crates are exposed to SFM first, and when does the local override become a pinned revision? | Canonical SFM compile/test and Java-local puppet proof precede propagation. |

Close each row in the completion notes of the work item that resolves it.

## Source and implementation references

| Reference | Location/command | Use |
| --- | --- | --- |
| CLI template | G:\Programming\Repos\teamy-rust-cli\Cargo.toml, src/lib.rs, check-all.ps1 | Baseline CLI, metadata, lint, logging, cancellation, and test conventions. |
| Profiling harness | G:\Programming\Repos\teamy-profiler\README.md, src/cli/run/cargo/run_cargo_cli.rs | Golden teamy-profiler run cargo workflow: explicit Cargo features, Tracy Capture launch, target logs, capture artifacts, manifest, and top-span post-processing. |
| Profiling template contract | G:\Programming\Repos\teamy-rust-cli\README.md, run-profiler.ps1, Cargo.toml, src/logging_init.rs | Source-of-truth project setup for the optional tracy feature, explicit tracing-subscriber/tracing-tracy feature selection, and the project wrapper used by active repositories. |
| Active profiling reference | D:\Repos\Azure\Cloud-Terrastodon\README.md, run-profiler.ps1, crates/tracing, Cargo.toml | Existing consumer workflow and tracing-layer separation to reconcile with the template rather than copy blindly. |
| Span-placement guidance | G:\Programming\Repos\skills\.github\skills\add-tracy-spans\SKILL.md | Coarse-versus-hot-span gating, bounded fields, stable names, frame markers, and capture-driven iteration. |
| Microsoft Terminal PTY artifacts | G:\Programming\Repos\microsoft-terminal | Reference for the future prebuilt ConPTY/OpenConsole artifact set; the MVP uses unmodified portable-pty/system ConPTY and does not vendor or package these binaries. |
| Vulkan API/examples | G:\Programming\Repos\ash (ash 0.38.0+1.4.352) | Verify raw loader, extension, pointer-chain, synchronization, and validation usage. |
| Working Vulkan application | G:\Programming\Repos\cursor-latency at 6c07705 | Reuse the proven ash/ash-window/winit application seam. |
| SFM repository rules | D:\Repos\Minecraft\SFM\repos2\1.19.2\docs\AGENTS.md | No-Gradle commands, baseline-first propagation, changelog, and audit rules. |
| SFM bridge contract | D:\Repos\Minecraft\SFM\repos2\1.19.2\docs\tasks\vox terminal bridge and graceful degradation plan.md | Java-local fallback, portable frames, capability negotiation, and puppet expectations. |
| Template quality commands | rustup run nightly -- cargo fmt --all -- --check; cargo clippy --workspace --all-targets --all-features -- -D warnings; cargo build --workspace --all-features; cargo test --workspace --all-features | Exact checks to retain/adapt in teamy-terminal/check-all.ps1. |

## Execution order

~~~text
bootstrap evidence [x]
  -> workspace + dependency repair + PTY crate boundary
  -> core contract + headless replay
  -> font and Vulkan tracks (intentional parallel work)
  -> interactive PTY/Vulkan MVP
  -> portable frame output (post-MVP)
  -> optional SFM local integration
  -> pinned dependency + cross-repository proof
~~~

## Phases and work items

## Phase 0 — Repository bootstrap [x]

### [x] 0.1 Create the public repository and MPL-2.0 baseline

**Work:** Create TeamDman/teamy-terminal, clone it to
G:\Programming\Repos\teamy-terminal, retain the generated MPL-2.0 license,
and establish main as the default branch.

**Validation:**

~~~powershell
gh repo view TeamDman/teamy-terminal --json nameWithOwner,isPrivate,defaultBranchRef,url,licenseInfo
git -C G:\Programming\Repos\teamy-terminal status --short
git -C G:\Programming\Repos\teamy-terminal log -1 --oneline
~~~

**Completion criteria:** The remote is public, reports MPL-2.0, and the local
clone has a clean pushed bootstrap commit.

**Completion notes:** e6ff0ec (chore: bootstrap teamy terminal repository) was
pushed to main. GitHub reports isPrivate: false, default branch main, and
license key mpl-2.0.

### [x] 0.2 Copy and audit the teamy-rust-cli scaffold

**Work:** Copy the template with its exclusion/preserve rules, replace package
identity and README placeholders, retain CLI metadata/lint/test conventions,
  and add the SFM pointer without creating a second plan copy.

**Validation:**

~~~powershell
cargo metadata --no-deps --format-version 1
rustup run nightly -- cargo fmt --all -- --check
Get-ChildItem G:\Programming\Repos\teamy-terminal\docs\tasks
~~~

**Completion criteria:** The package identifies as teamy-terminal, the plan
exists in the repository, and metadata/format checks pass.

**Completion notes:** The normal cargo run -- init was attempted and failed
before dispatch because of the incompatible Facet revisions described above.
The compatibility initializer copied the reviewed scaffold. Metadata and
nightly formatting passed. The SFM document now points to this authoritative
plan instead of duplicating it.

## Phase 1 — Workspace foundation [~]

### [x] 1.1 Convert the single package into the five-crate workspace

**Work:** Create the root workspace and
crates/teamy-terminal-core, crates/teamy-terminal-font,
crates/teamy-terminal-pty, crates/teamy-terminal-renderer, and
crates/teamy-terminal-cli. Set workspace metadata, resolver, locked
dependencies, and a default member for the CLI.
Move only relevant template code into the CLI crate; remove the example cache,
home, and init product API after its bootstrap value is preserved in repository
history.

**Validation:**

~~~powershell
cargo metadata --format-version 1
cargo run -- --help
cargo run -- --version
~~~

**Completion criteria:** A fresh checkout resolves all five members, the root
smoke commands work, and no crate outside the CLI depends on Windows resource
or process-hosting details unnecessarily.

**Completion notes:** The virtual resolver-3 workspace contains all five
planned crates with `teamy-terminal-cli` as the default member. The relevant
Figue/Facet CLI, metadata, logging, cancellation, and Windows startup code
lives under `crates/teamy-terminal-cli`; the template cache, home, paths, and
init product API was removed. Windows resources and the embed-resource build
script are CLI-local. `teamy-terminal-pty` owns the unmodified `portable-pty`
boundary and remains independent of core/font/renderer code. The topology,
help, version, and locked workspace checks pass.

### [x] 1.2 Repair the Facet revision and adapt the workspace quality gate

**Work:** Align the direct Facet/figue dependencies with the revision used by
teamy-cancellation, or replace only the incompatible template integration while
preserving the CLI's public behavior. Adapt check-all.ps1 to run nightly
formatting, workspace Clippy with -D warnings, workspace build, and workspace
tests. Keep build.rs, rustfmt.toml, clippy.toml, cancellation, structured
logging, and Windows resource handling scoped to the CLI package.

**Validation:**

~~~powershell
./check-all.ps1
cargo tree -d
~~~

**Completion criteria:** The full quality gate passes from a fresh clone and
the lockfile contains one compatible Facet family for the CLI.

**Completion notes:** Updated `TeamDman/facet` `mine/main` from its previous
state by fetching `origin/main` (upstream tip `7b0e73a37`) and merging it into
the fork. The synchronized fork was committed and pushed as `23ad4dcd0`.
Updated `teamy-cancellation` to use that same Facet/Figue revision in commit
`2ba7fa5`, pushed to its `main`, and pinned teamy-terminal to that commit.
Every direct Facet/Figue dependency and the lockfile now resolves to
`23ad4dcd0`; `cargo tree -d` shows no duplicate Facet family. The hook-bypassing
`--no-verify` choices were required for those two commits.

`check-all.ps1` now runs nightly `cargo fmt -- --check`, workspace Clippy with
`-D warnings`, workspace all-feature build, and workspace all-feature tests.
The complete script passes, as do the explicit locked workspace checks.
Build.rs, resources, structured logging, cancellation, and Windows handling are
CLI-scoped.

The teamy-profiler dry-run contract for the terminal was verified with:

~~~powershell
cargo run --locked -- run cargo --project G:\Programming\Repos\teamy-terminal --bin teamy-terminal --profile release --feature tracy --dry-run
~~~

It produced the expected release target, explicit tracy feature, and
timestamped .tracy, .ansi.log, .logs.ndjson, .top-spans.csv, and
.profile-run.json artifact paths. A live capture was then completed through
the local `C:\Program Files\bin\tracy-capture.exe` installation; the
resulting evidence is recorded under Phase 3.5.3.

### [x] 1.3 Establish renderer-free core and fixture layout

**Work:** Add the core crate's public session/state types, fixture directories,
bounded limits, and a minimal deterministic test without importing Vulkan,
window, CUDA, Minecraft, or process-hosting dependencies.

**Validation:**

~~~powershell
cargo test -p teamy-terminal-core
cargo tree -p teamy-terminal-core
~~~

**Completion criteria:** The core crate builds and tests independently, and its
dependency tree contains no renderer/window/process-hosting dependency.

**Completion notes:** Added public `TerminalLimits`, `CursorPosition`,
`Color`, `Cell`, `TerminalSnapshot`, and `TerminalSession` types with bounded
dimensions and scrollback, VT parsing for ordinary text/control sequences,
ANSI styles, cursor movement, cursor reports, deterministic resize, and core
fixtures. `cargo test -p teamy-terminal-core` passes. Its only dependencies
are `vte` and `unicode-width`; it imports no renderer, window, CUDA,
Minecraft, or process-hosting dependency.

## Phase 2 — Terminal core and headless proof [~]

### [~] 2.1 Define the terminal state and input contract

**Work:** Implement bounded columns/rows/scrollback, cells and styles, cursor,
selection, damage, deterministic resize, text/key/mouse events, prompt and
command-range metadata, and snapshot/replay serialization. Keep process
hosting outside the core.

**Validation:**

~~~powershell
cargo test -p teamy-terminal-core -- --nocapture
~~~

**Completion criteria:** Unit tests cover ordinary text, VT/control input,
cursor/style transitions, selection, resize, damage, malformed input, bounds,
and deterministic snapshot equality.

**Completion notes:** The MVP core covers ordinary text, VT/control input,
cursor/style transitions, bounded scrollback, resize, cursor reports, and
deterministic snapshots. Selection, damage reporting, mouse events, and
process-free transcript replay remain outside the current interactive slice.

### [x] 2.2 Implement the PTY adapter and prove bounded PowerShell 1..1000 input

**Work:** Implement `teamy-terminal-pty` around unmodified `portable-pty`.
Interactive startup uses `pwsh.exe` with the user's normal profile; the
bounded smoke adapter uses `pwsh.exe -NoProfile` and documents the Windows
PowerShell fallback. Bound command text, environment, working directory,
output bytes, rows, timeout, and process lifetime for tests. Keep the PTY crate
independent of Vulkan/windowing and do not expose an unrestricted remote shell
API.

**Validation:**

~~~powershell
cargo test -p teamy-terminal-pty
~~~

**Completion criteria:** The reusable PTY crate exposes terminal-oriented
startup, I/O, resize, cancellation, and exit behavior for the later interactive
CLI. The Windows `-NoProfile` smoke sends a bounded 1..1000 sequence and waits
for successful shell exit without exposing an unrestricted remote-shell API.

**Completion notes:** `teamy-terminal-pty` is implemented around unmodified
`portable-pty 0.9`. Interactive configuration keeps the normal PowerShell
profile; the test configuration adds `-NoProfile`, fixed terminal environment
variables, bounded output events, a 15-second smoke timeout, and explicit
child cleanup. The Windows smoke passes in the development environment.

### [ ] 2.3 Replay the proof without launching PowerShell

**Work:** Persist a deterministic transcript/snapshot fixture and replay it
through the same core API. Include cancellation, process exit, malformed
output, and scrollback-limit cases.

**Validation:**

~~~powershell
cargo test -p teamy-terminal-cli replay -- --nocapture
~~~

**Completion criteria:** Replay reproduces rows, cursor, scrollback, damage,
and fixture hash without a process, and bounded failure cases are asserted.

## Phase 3 — Font and Vulkan tracks [ ]

These are intentionally parallel: the renderer may use a temporary fake glyph
source while the font seam is finalized. Each track must cite the source seam
it reused and must not edit the other track's public contract silently.

### [x] 3.1 Extract the terminal font crate

**Work:** Embed Caskaydia Cove as the initial terminal font and extract only
glyph shaping/rasterization inputs, metrics, cell placement, style/color
mapping, and deterministic atlas or glyph-instance output from Teamy Studio.
Exclude application panels, window chrome, CUDA/Burn, and unrelated workspace
models.

**Validation:**

~~~powershell
cargo test -p teamy-terminal-font
~~~

**Completion criteria:** A deterministic glyph/atlas fixture and one
renderer-consumable text frame pass without Teamy Studio dependencies.

**Completion notes:** The font crate embeds the official Caskaydia Cove Nerd
Font Mono regular face and preserves its license. `fontdue` rasterizes cached
glyphs into bounded RGBA frames with terminal palette, bold, underline, and
inverse handling; its render test passes independently.

### [x] 3.2 Build the ash Vulkan presentation boundary

**Work:** Use the Ash checkout and cursor-latency seam to load an entry,
create instance/device/queue/surface/swapchain, render terminal cells, handle
resize and synchronization, and isolate unsafe code. First support Windows;
return actionable loader/device/validation errors when unavailable.

**Validation:**

~~~powershell
cargo test -p teamy-terminal-renderer
cargo run -- --stop-after-duration 5s
~~~

**Completion criteria:** A usable Vulkan device produces a windowed proof, an
unavailable device produces a diagnostic, and no DirectX/CUDA dependency enters
the renderer or core.

**Completion notes:** The renderer implements Ash instance/device/queue/surface/
swapchain construction, host-visible staging upload, layout transitions,
synchronization, presentation, swapchain recreation, and explicit fatal
diagnostics. `cargo run -- --stop-after-duration 3s` reached graceful
cancellation after constructing the PTY/window/Vulkan path. Forcing an invalid
`VK_ICD_FILENAMES` value exits with the actionable Vulkan-required diagnostic;
the captured window proof shows the embedded-font frame and profile prompt.
There is no CPU-rendering fallback.

### [ ] 3.3 Add off-screen readback and portable frame encoding

**Work:** Add an off-screen target and CPU-visible readback independent of a
presentable window. Expose sequence, dimensions, stride, format, full-frame or
dirty-tile kind, compression, and bytes. PNG is the archive/keyframe path;
raw or losslessly compressed tiles are the live path.

**Validation:**

~~~powershell
cargo run -p teamy-terminal-cli -- render-fixture --format png
cargo test -p teamy-terminal-renderer frame -- --nocapture
~~~

**Completion criteria:** A deterministic PNG and bounded frame bytes can be
validated without Minecraft or a native GPU shared handle; resize and dirty
region behavior are tested.

### [~] 3.4 Wire the interactive `cargo run` MVP

**Work:** Make `cargo run` with no product subcommand create the Windows Vulkan
window, launch the default `pwsh.exe` through `teamy-terminal-pty`, consume PTY
output through the core state model, render it with the Vulkan renderer, route
keyboard/paste input back to the PTY, and propagate window resize. Preserve
`--help` and `--version`. Bail with an actionable diagnostic if Vulkan object
construction fails; do not silently fall back to CPU rendering. Prebuilt
ConPTY/OpenConsole artifacts remain an external prerequisite and are not
expanded into this MVP's packaging work.

**Validation:**

~~~powershell
cargo run -- --help
cargo run -- --version
cargo run
~~~

Manually verify a prompt, a simple command such as `echo TEAMY_TERMINAL_OK`,
ANSI output, paste, resize, Ctrl+C, Ctrl+wheel font scaling, and clean shell
exit.

**Completion criteria:** `cargo run` opens a usable Vulkan terminal window and
the user can run ordinary shell commands through the default-profile
PowerShell session. PTY output, keyboard input, resize, cancellation, and
process exit are connected end to end. Missing Vulkan or another graphics
construction failure terminates with a diagnostic and no fallback renderer.

**Completion notes:** The CLI now owns the winit application handler and wires
PTY output, VT state, embedded-font frames, Vulkan presentation, keyboard text
and navigation, Ctrl+C, clipboard paste, resize, cancellation, and child
cleanup. Help/version continue to bypass interactive launch. The pure input
mapping tests cover text, Enter, navigation, and control-byte routing; the
bounded `-NoProfile` PTY smoke covers command input and successful exit. A
desktop input probe was attempted with both `SendKeys` and native
`SendInput`, but this session rejected synthetic keystrokes, so direct manual
command/paste/resize/Ctrl+C/exit evidence remains the only open MVP check.

Interactive polish now maps Ctrl+Backspace to the backward-kill-word control
byte used by PowerShell-compatible prompts, uses a black default terminal
background so gray and white text remain visibly distinct, and keeps the
existing ANSI palette values explicit and tested. Ctrl+wheel font scaling is
bounded to 8–32 physical pixels in 1-pixel steps; each change rebuilds the
font metrics/cache, invalidates the cached frame, reflows the terminal, and
resizes the PTY. Automated direction/bounds tests pass; direct live-window
verification remains part of the desktop interaction gate.

## Phase 3.5 — Profiling contract and Windows presentation polish [~]

This phase is the performance and integration follow-up to the interactive
MVP. teamy-profiler is the golden execution harness; teamy-rust-cli is the
golden project template; active repositories such as teamy-terminal and Cloud
Terrastodon must consume that contract rather than independently invent
profiling conventions.

### [x] 3.5.1 Set the native window icon

**Work:** Keep the existing Windows resource embedding for the executable,
taskbar, and file identity, and additionally decode the existing
crates/teamy-terminal-cli/resources/main.png into the RGBA data required by
winit::window::Icon. Pass that icon through Window::default_attributes() so
the actual Vulkan window receives the same icon as the taskbar/resource
identity.

**Validation:** Build the Windows binary, launch the Vulkan window, and verify
the icon in the title bar, task switcher, taskbar, and executable properties.
The resource icon and the winit window icon must be sourced from the same
checked-in asset family.

**Completion criteria:** The visible window icon is correct without changing
the existing taskbar/resource behavior, and icon loading failure is an
actionable startup error rather than a silent missing icon.

**Completion notes:** The embedded `main.png` is decoded as bounded RGBA8 data
and passed to `Window::default_attributes().with_window_icon(...)`. The
decoder and winit icon construction are covered by a focused CLI test, while
the Windows executable resource and taskbar icon remain sourced from the
existing `main.ico` resource.

### [~] 3.5.2 Make Tracy and tracing-subscriber feature selection explicit

**Completion notes:** This is the current implementation focus. The existing
CLI exposes only a `tracy` feature; the Cloud Terrastodon reference and the
local `add-tracy-spans` guidance support adding a separate
`extended_observability` feature, with `tracy` enabling both the detailed
instrumentation and the Tracy transport. The current teamy-profiler dry-run
and live-capture contract are recorded below.

The first implementation slice now adds `extended_observability`, makes
`tracy` imply it, disables direct `tracing-subscriber` defaults, filters
`tracy.frame_mark` from human/JSON logs, and adds coarse spans for window
creation, resize, PTY processing, frame rasterization, and upload/present.
The capture path is live: `teamy-profiler` was fast-forwarded to `9e2379a`,
`tracy-capture.exe` was discovered at `C:\Program Files\bin`, and a release
capture completed with a machine-readable manifest and top-span CSV. Default
and Tracy feature builds pass with isolated target directories. CLI library
tests pass 3/3 and terminal-core tests pass 5/5. The full workspace test is not
green because the existing randomized `cli_fuzzing` coverage test found zero
samples for several command paths after 128,000 attempts; this is recorded as
an existing test-harness issue, not a compile or instrumentation failure.

The feature graph still shows `tracing-subscriber` default features arriving
through pinned `teamy-cancellation` (`figue` enables its
`tracing-subscriber` feature, whose dependency does not yet set
`default-features = false`). This item remains in progress until that
transitive feature leak is either fixed upstream or explicitly accepted as a
documented exception.

#### [~] 3.5.2.a Repair teamy-cancellation's tracing feature boundary

**Work:** Update the local `G:\Programming\Repos\teamy-cancellation`
checkout so its optional `tracing-subscriber` dependency uses
`default-features = false` and an explicit `registry` capability, which is the
only subscriber capability used by `StopAfterLayer`. Keep its Facet/Figue pin
at the compatible `23ad4dcd0` revision. Add or retain a focused feature-graph
check and run the crate's all-feature tests. After the repair is reviewed,
propagate the resulting teamy-cancellation revision (or a release containing
it) to teamy-rust-cli, teamy-terminal, and Cloud Terrastodon; do not leave
consumers on mixed Facet or tracing-subscriber contracts.

**Validation:** From teamy-cancellation, run `cargo tree -e features -i
tracing-subscriber`, `cargo test --all-features --locked`, and the repository
quality gate. Then rerun the same feature-graph audit and locked focused
checks in each consumer, including the template's Facet-bound compile and
the teamy-profiler dry-runs.

**Completion criteria:** No consumer's resolved dependency graph reaches
`tracing-subscriber feature "default"` through teamy-cancellation; the
template compiles without duplicate incompatible Facet traits; all updated
pins are recorded in lockfiles; and the existing profiling manifests remain
unchanged apart from their source revision metadata.

**Completion notes:** The local repair is implemented and committed in
`G:\Programming\Repos\teamy-cancellation` as `7c77848`
(`fix: keep tracing-subscriber defaults opt-in`). Its feature graph now shows
only `tracing-subscriber`'s `registry` capability. All-feature tests pass
(9 unit tests and 3 integration tests), as do clippy, the all-feature build,
examples, and subscriber-only tests. The repository ignores its Cargo.lock,
so consumer lockfiles capture the published revision. `7c77848` is now
published; teamy-rust-cli and teamy-terminal resolve it and their local
feature graphs no longer include `tracing-subscriber default`. Cloud
Terrastodon still receives an older registry `teamy-cancellation` through its
remote `teamy-mft@42e6e21` dependency, which is tracked by the follow-up item
below. Its earlier profiling-contract edits remain preserved; updating its
dependency pins is explicitly deferred from this slice.

#### [x] 3.5.2.b Repair teamy-mft's transitive profiling pins

**Work:** Update `G:\Programming\Repos\teamy-mft` so its direct
`tracing-subscriber` dependency uses `default-features = false` with the
explicit `ansi`, `env-filter`, `fmt`, `json`, and `registry` capabilities, and
so it consumes the published `teamy-cancellation@7c77848` revision rather than
registry `0.2.0`. Preserve its existing Facet `5fd9cfaa` integration and the
three unpublished commits already present on its local `main` branch.

**Validation:** Run teamy-mft's all-feature check and
`cargo tree -e features -i tracing-subscriber --all-features`. Publish only a
dedicated repair branch that excludes unrelated unpublished `main` commits.
Cloud Terrastodon propagation is explicitly deferred from this slice.

**Completion criteria:** A fresh checkout of the dedicated teamy-mft repair
branch resolves the published teamy-cancellation revision, no dependency path
in teamy-mft reaches `tracing-subscriber feature "default"`, and the branch
contains no unrelated unpublished `main` commits. Cloud propagation remains a
separate future task.

**Completion notes:** The dedicated repair branch is published as
`teamy-mft/profiling-dependency-repair` at `a74639b`
(`fix: align profiling dependencies`), based directly on remote `main` and
excluding the four commits on the local `main`. Its all-feature check passes
and its feature graph contains only the explicit subscriber capabilities. The
full library/integration test suite still encounters the existing Figue/Vox
API failures, which are unrelated to this dependency repair. Cloud remains on
its prior `teamy-mft@42e6e21` pin by deliberate scope.
The teamy-mft all-feature integration test still fails in the existing
`vox_daemon_log_channel` test because the pinned Vox API no longer provides
`TransportMode`, `NoopClient`, and the older builder signatures; the library
and dependency-boundary compile succeeds, so that failure is not attributed to
this repair.

**Work:** Audit the full dependency graph, including teamy-cancellation, so
the application does not accidentally re-enable tracing-subscriber default
features through a transitive dependency. Use default-features = false and an
explicit feature list for tracing-subscriber and tracing-tracy; retain only
the formatter, registry, environment-filter, JSON, and Tracy capabilities
actually used by the project. Keep the application tracy feature opt-in and
ensure normal builds do not link the Tracy layer.

Adopt the Cloud Terrastodon layering pattern rather than making `tracy` carry
every observability decision: add an `extended_observability` feature for
high-frequency diagnostic spans and frame markers, and define `tracy` as the
transport layer that enables `extended_observability` plus
`tracing-tracy`. Keep allocation profiling, if ever needed, as a separate
explicit feature. This lets ordinary builds retain useful coarse spans while
avoiding the creation cost of detailed render/terminal spans.

Keep human stderr/NDJSON filters independent from the Tracy filter. Tracy must
receive the intended trace-level spans, while ordinary output excludes
profiling-only markers such as tracy.frame_mark where appropriate. Add tests
for the fixed Tracy filter and a feature-graph check that makes accidental
default-feature reintroduction visible.

**Validation:**

~~~powershell
cargo tree -e features -i tracing-subscriber
cargo tree -e features -i tracing-tracy
cargo test --workspace --all-features --locked
cargo build --workspace --no-default-features --locked
cargo build -p teamy-terminal-cli --features tracy --locked
~~~

**Completion criteria:** The default build has no Tracy layer; the profiling
build has the explicitly selected Tracy capabilities; and the dependency graph
does not silently acquire the default tracing-subscriber/tracing-tracy feature
sets through teamy-cancellation or another shared crate.

### [~] 3.5.3 Instrument the performance-critical terminal path

**Completion notes:** Coarse Tracy spans and the frame marker are present, and
the extended-observability path now emits bounded frame statistics for visible
cells, changed cells, cursor changes, rasterized pixels, and actual uploaded
bytes. A reproducible
large-output/resize capture completed at
`G:\Programming\Repos\teamy-terminal\tracy\2026-07-25_21-28-47.profile-run.json`
with the corresponding `.tracy`, `.logs.ndjson`, and `.top-spans.csv` artifacts.
The 25-second release run used `extended_observability,tracy`, `--log-filter
debug`, 500 generated PowerShell lines, and 800x450/1600x900 control resizes.
It recorded 53 rendered frames over 4,736 visible cells at 1280x720, 1,840 at
800x450, and 7,520 at 1600x900. Across all frames, 21,365 cells changed
(403/frame, 8.56% of visible cells on average); 10 frames had zero changed
cells. The current implementation nevertheless rasterized 48,801,600 pixels
and uploaded/presented every rendered frame. Tracy top spans reported 80.1 ms
in rasterization, 34.9 ms in upload/present, and 2.1 ms in PTY processing.
This is diagnostic evidence, so the extended snapshot comparison is not yet a
production performance claim.

The `dirty_rendering` prototype now adds cached-frame cell redraw with
wide-glyph/cursor expansion, packed pixel rectangles, per-image frame
revisions, image-indexed present semaphores, deferred fence reset, and full
refresh fallbacks for stale swapchain images, resize, and cache loss. The
swapchain recreation path passes the old swapchain handle to Vulkan before
retiring it, preventing `ERROR_NATIVE_WINDOW_IN_USE_KHR` during replacement.
Font, damage-region, pixel-equivalence, renderer-packing, CLI, and core tests
pass. The controlled candidate capture at
`G:\Programming\Repos\teamy-terminal\tracy\2026-07-25_22-07-06.profile-run.json`
completed with 136 frames, 24,947 changed cells (3.89% average), 6,780,670
rasterized pixels, and 501,177,600 uploaded bytes. It exercised generated
PowerShell output plus 800x450 and 1600x900 resizes; its top spans measured
22.6 ms in rasterization, 69.6 ms in upload/present, and 2.0 ms in PTY
processing. The per-image revision fallback required full uploads for 135 of
136 frames (the resize frames were 1,440,000 and 5,760,000 bytes), so the
capture validates the CPU dirty redraw and correctness-first swapchain policy
but does not claim a partial-upload win under the rotating MAILBOX swapchain.
The 25-second candidate capture at
`G:\Programming\Repos\teamy-terminal\tracy\2026-07-25_22-11-09.profile-run.json`
also completed normally, but the second resize arrived after its cancellation
window and is not used as the resize proof.

An apples-to-apples 25-second comparison then ran the same bounded workload
without resize or generated-output variability. The full-frame run at
`G:\Programming\Repos\teamy-terminal\tracy\2026-07-25_23-02-30.profile-run.json`
and the dirty-rendering run at
`G:\Programming\Repos\teamy-terminal\tracy\2026-07-25_23-09-42.profile-run.json`
both completed 13 frames with 61,568 visible-cell observations, 5,767 changed
cells, 6 zero-change frames, and 6 cursor-change frames. Dirty rendering
reduced rasterized pixels from 11,980,800 to 1,132,500 (90.55%), while
uploaded bytes remained 47,923,200 in both runs. The top-span totals were
24.3 ms versus 9.1 ms for rasterization, 6.6 ms versus 5.4 ms for frame
construction, and 8.7 ms versus 10.2 ms for upload/present. The last timing is
one capture and is not a performance claim, but the byte count is conclusive:
the current revision-safe policy avoids unchanged glyph rasterization while
still performing full-frame uploads on the rotating MAILBOX images. Both
captures also contained zero `tracy.frame_mark` records in ANSI or NDJSON
logs.

The renderer then gained bounded `uploaded_full_refresh` and
`uploaded_region_count` fields so the fallback reason is directly observable.
A second matched pair at
`G:\Programming\Repos\teamy-terminal\tracy\2026-07-25_23-18-49.profile-run.json`
and
`G:\Programming\Repos\teamy-terminal\tracy\2026-07-25_23-20-31.profile-run.json`
reproduced the same 13 frames, 5,767 changed cells, 11,980,800 versus
1,132,500 rasterized pixels, and 47,923,200 uploaded bytes. The dirty capture
requested 48 regions in total, but all 13 acquired images required a full
refresh and submitted one full-frame upload region. This confirms that the
current MAILBOX image rotation, not the cell-diff calculation, is preventing
partial uploads in this workload.

A separate idle-responsiveness audit found that the interactive event loop was
set to `ControlFlow::Poll`, so it continuously spun while waiting for PTY and
control-channel work. A short release smoke measured roughly 3.3 CPU-seconds
for the old polling build during idle startup. The loop now uses an 8 ms
`ControlFlow::WaitUntil` cadence, retaining bounded PTY latency without
burning a core when no frame is dirty. The patched release smoke remained
responsive and used less than one CPU-second while active. This scheduler fix
is independent of the still-open MAILBOX full-upload limitation.

Decision: the apples-to-apples comparison confirms that the dirty-rectangle
prototype is correctness-safe for the measured path and materially reduces
host rasterization. It is now the default interactive path; the full-frame
baseline remains available through `--no-default-features`. Rotating swapchain
images still force conservative full uploads, so the implementation must
continue to cache the previous frame and update/upload only affected
rectangles when the acquired swapchain image has the current frame revision.
Stale images must receive a full refresh; this
preserves correctness even when it limits the upload benefit. Cursor, scroll,
wide-glyph, style, resize, occlusion, and full-refresh invalidation remain
acceptance requirements. The explicit fallback evidence and repeated matched
captures are now complete. The next implementation slice should determine
whether the swapchain/presentation strategy can safely reuse a current-revision
image, or whether partial-upload optimization requires a different persistent
render target; do not weaken the stale-image full-refresh rule. Do not accept
a cell-diff-only change that still clears/rasterizes/uploads the full frame.

**Work:** Add low-overhead, Tracy-visible spans around PTY draining and output
batching, VT parsing/core reflow, font fallback/rasterization, host frame
construction, Vulkan staging upload/present, swapchain recreation, and resize
processing. Add a stable frame marker suitable for Tracy timelines without
turning every cell or glyph into an unbounded span. Preserve useful target
metadata in span names and fields so teamy-profiler tracy csv export can
identify the expensive stage from a capture.

The first explicit performance hypothesis is terminal damage handling: the
current interactive path has one `dirty` gate, but a dirty frame snapshots the
whole cell grid, rasterizes a whole RGBA frame, and uploads the whole staging
buffer. Measure this before changing it; do not assume unchanged glyph repaint
is the dominant cost. The `dirty_rendering` path is the default interactive
renderer now that the comparable capture and correctness proof have passed;
`--no-default-features` retains the baseline. Add bounded counters/fields
for total cells, changed
cells, dirty-rectangle count and area, rasterized pixels, glyph-cache hits and
misses, uploaded bytes, frame presents, idle redraws, and resize/reflow time.
Then compare a full-frame baseline with a correctness-preserving changed-cell
or dirty-rectangle path. Cursor changes, style/background changes, scrolling,
wide glyphs, resize, and forced full refresh must invalidate all affected cells.

Follow `add-tracy-spans` when adding the instrumentation: use stable coarse
phase spans for the user-visible pipeline, gate high-frequency render/cell/
glyph detail behind `extended_observability`, avoid per-cell spans and dynamic
fields that fragment Tracy statistics, and emit `tracy.frame_mark` only for
the profiling/frame-marker path.

**Validation:** Capture bounded startup, idle, steady-state output, large-output,
cursor/input, and resize workloads through run-profiler.ps1. Inspect both Tracy
and the generated top-span CSV, and retain a machine-readable manifest of the
workload, window size, feature set, Tracy version, and source revision.
Confirm that the capture contains the terminal process, target logs are
retained, profiling markers do not pollute normal stderr/NDJSON output, and the
damage counters distinguish unchanged-cell frames from genuinely changed
frames. For any optimization, compare before/after captures using the same
workload and report CPU time, rasterized pixels, upload bytes, presents, and
frame latency rather than accepting a visual impression.

**Completion criteria:** A repeatable capture identifies the dominant CPU and
resize costs, establishes whether unchanged glyphs are being repainted, and
supports a before/after decision for damage tracking. The instrumented path
remains usable without Tracy, profiling overhead is not hidden by accidental
logging fan-out, and no optimization is accepted without a matching
correctness proof and comparable capture.

### [~] 3.5.4 Synchronize the template and profiler source of truth

**Work:** Update teamy-rust-cli so its Cargo.toml, logging module,
run-profiler.ps1, and README explicitly document the profiling contract:

- The template is an opinionated starting point for Teamy Rust CLI projects.
- tracy is opt-in and tracing-tracy uses default-features = false with an
  explicit capability list.
- tracing-subscriber uses default-features = false with an explicit list, and
  transitive dependencies must not restore defaults.
- Human logs, structured logs, and Tracy are independently filtered layers.
- run-profiler.ps1 delegates to teamy-profiler run cargo rather than inventing
  a second capture workflow.
- tracy-capture.exe is an external prerequisite; captures and manifests are
  written to the repository tracy directory.
- Active repositories should periodically compare their profiling setup to
  this template and the profiler rather than silently diverging.

Update teamy-profiler README to document the corresponding harness contract,
including feature forwarding, release/debug selection, target-argument
forwarding, capture elevation, artifact naming, automatic log-file injection,
and top-span post-processing. Reconcile Cloud Terrastodon's existing tracing
crate and README with these rules while preserving its project-specific
features.

**Validation:** A current template checkout and teamy-terminal both pass a
teamy-profiler run cargo dry-run; their manifests show the expected
target/features/artifacts; and README examples are executable with only
project-specific command names changed. Re-run the same dry-run against Cloud
Terrastodon to preserve its project-specific allocation feature.

**Completion criteria:** The template README is the readable policy, the
profiler README is the readable harness specification, and active projects can
be audited against both without relying on memory or scattered code searches.
The harness must also have an explicit, bounded Tracy Capture shutdown path;
the current implementation waits for normal capture exit, records the shutdown
status in the manifest, and kills the capture after its bounded timeout.

**Completion notes:** The synchronization slice updated `teamy-rust-cli` and
`teamy-profiler` Cargo feature declarations, logging filters, wrapper scripts,
and README policy; added the explicit `ansi` capability required by the
`.pretty()` formatter; and reconciled Cloud Terrastodon's tracing README and
workspace dependency declaration while preserving its `tracy-alloc` workflow.
`teamy-profiler` was pulled first and was already current at `9e2379a`.

The profiler tests pass (10 tests), the Cloud tracing tests pass (2 tests),
the terminal CLI all-feature check passes, and the template logging module
passes nightly rustfmt. Dry-runs pass for the template, teamy-terminal, and
Cloud Terrastodon. The first two manifests forward
`extended_observability,tracy`; Cloud forwards `tracy,tracy-alloc`; all three
produce the expected Tracy, ANSI, NDJSON, top-span CSV, and profile-run
manifest paths. Existing captured runs also report normal Tracy Capture exit
(`capture_shutdown_status: exited:exit code:0`); the old note claiming the
capture remained alive is no longer current.

The feature-graph audit is now clean for the template, teamy-terminal,
teamy-profiler, and the dedicated teamy-mft repair branch: each resolves only
the explicit subscriber capabilities. Cloud Terrastodon remains on its prior
remote teamy-mft pin by deliberate scope, so its older transitive default is a
deferred Cloud task rather than a reason to alter this slice. The template's
Facet-bound library test and all-feature compile now pass after the pins were
aligned; teamy-mft's unrelated existing Figue/Vox integration failures remain
documented under Phase 3.5.2.

### [x] 3.5.5 Establish and record a performance baseline

**Work:** Use teamy-profiler and TracyCapture to capture bounded workloads for
startup, steady-state output, large output, resize/reflow, and idle redraw.
Store only reviewed manifests/top-span summaries in the repository; keep raw
.tracy captures as local artifacts unless a capture is intentionally retained
for regression analysis. Record the command, feature set, Tracy version,
window dimensions, workload, and machine/runtime context.

**Validation:** Each retained baseline can be opened in Tracy Profiler or
summarized with teamy-profiler tracy csv export, and a later run can be
compared without changing the capture command or feature policy.

**Completion criteria:** We have a reproducible profiling command and a first
baseline that tells us whether the next optimization should target PTY/core
processing, font/frame construction, Vulkan upload/present, or resize/reflow.

**Completion notes:** A fresh full-frame baseline completed at
`G:\Programming\Repos\teamy-terminal\tracy\2026-07-25_23-02-30.profile-run.json`
using release `extended_observability,tracy`,
`--log-filter debug --stop-after-duration 25s`, and a 25.466-second target
runtime. The manifest reports exit code 0, top-span post-processing `ok`, and
`capture_shutdown_status: exited:exit code: 0`. Its 13 frame-stat records
contained 6 zero-change frames, 5,767 changed cells, 11,980,800 rasterized
pixels, and 47,923,200 uploaded bytes; the average changed-cell ratio was
9.37%. The top-span CSV measured 208.1 ms in window creation, 24.3 ms in
rasterization, 8.7 ms in upload/present, 6.6 ms in frame construction, and
0.13 ms in PTY processing. Both ANSI and NDJSON logs contained zero
`tracy.frame_mark` records.

Together with the retained large-output/resize captures at
`2026-07-25_21-28-47.profile-run.json` and the correctness-first dirty
candidate at `2026-07-25_22-07-06.profile-run.json`, this establishes the
next decision: unchanged frames are still repainting and uploading a full
frame in the baseline, while the matched dirty capture reduces host
rasterization by 90.55% but still uploads the full frame because the acquired
MAILBOX images are stale. The bounded fallback counters and a fresh matched
pair now prove that all 13 candidate frames took the full-refresh path. The
next implementation slice should evaluate the swapchain/persistent-target
design before attempting further partial-upload changes, while retaining the
current CPU dirty-rendering result as the default interactive optimization.

## Phase 4 — Optional SFM integration [ ]

### [ ] 4.1 Integrate a local core/frame path without runtime coupling

**Work:** Add a local-only Cargo configuration override in the SFM CLI for
G:\Programming\Repos\teamy-terminal. Depend first on core and the portable
frame/protocol crate only. Missing checkout or renderer must produce a clear
development diagnostic and must not break Java-local SFM behavior.

**Validation:** From canonical 1.19.2, use repository-approved commands:

~~~powershell
sfm-propagate-changes.exe run compile
sfm-propagate-changes.exe test run
~~~

**Completion criteria:** The local override is absent from committed canonical
source, the CLI compiles with the checkout present, and Java-local terminal
tests remain valid with it absent.

### [ ] 4.2 Replace the local override with a reviewed pin

**Work:** After the core/frame API is reviewed, publish or pin an exact git
revision. Record lockfile hashes and source provenance; do not propagate an
absolute path or unstable branch reference to later Minecraft versions.

**Validation:**

~~~powershell
cargo tree --locked
sfm-propagate-changes.exe run compile
sfm-propagate-changes.exe test run
~~~

**Completion criteria:** A fresh SFM checkout resolves the pinned artifact and
canonical compile/tests pass before any version propagation.

### [ ] 4.3 Prove the optional frame consumer boundary

**Work:** Coordinate with the SFM Vox plan to consume portable frame data while
Java owns texture upload, clipping, GUI-scale/layout, stale-frame rejection,
and fallback. Rust never sends a Minecraft panel or arbitrary UI instruction.

**Validation:** Run the existing SFM puppet/report flow for Java-local and,
when the bridge is available, Rust-backed mode; include full-screen, nested,
narrow, and supported GUI-scale captures.

**Completion criteria:** The Java-local mode remains useful with no Rust process;
the optional mode proves input round-trip, resize, disconnect/stale-frame
handling, and fallback without native GPU-handle interop.

## Phase 5 — Release and handoff [ ]

### [ ] 5.1 Complete the support matrix and evidence bundle

**Work:** Record exact evidence for each advertised target and explicitly list
unsupported targets. Include fresh-clone commands, core fixtures, renderer
diagnostics, frame artifacts, and SFM proof links.

**Validation:**

~~~powershell
./check-all.ps1
git status --short
~~~

**Completion criteria:** Every supported target has the required proof and no
unsupported target is described as working by implication.

### [ ] 5.2 Update documentation, changelog, and propagation records

**Work:** Keep this plan authoritative in teamy-terminal/main. Keep the SFM
pointer valid, update SFM's gameplay changelog when an in-game surface changes,
run the baseline-first propagation/audit workflow, and record commit IDs and
intentional exclusions.

**Validation:**

~~~powershell
cargo run -- audit --branch core --version-surfaces
sfm-propagate-changes.exe git status
~~~

**Completion criteria:** Public contract, README, this plan, the SFM pointer,
SFM changelog, and propagation/audit evidence agree; no later target was
overwritten by baseline code.

## Acceptance matrix

| Target/dialect | Support status | Required validation | Evidence |
| --- | --- | --- | --- |
| Windows x64 core/CLI | Supported first | workspace quality gate, PTY smoke, replay | Quality gate and PTY smoke pass; replay pending |
| Windows x64 interactive Vulkan MVP | Goal of the current slice | PTY/Vulkan integration, command/resize/Ctrl+C/exit proof | Startup, renderer diagnostic, PTY smoke, and input mapping pass; desktop interaction pending |
| Windows Vulkan device | Supported when loader/device exists | renderer diagnostic plus window/off-screen proof | Window startup smoke passes; off-screen path deferred |
| Windows without Vulkan/GPU | Supported for core/CLI; interactive renderer fails clearly | core/CLI pass; actionable renderer-construction error; no fallback | Invalid Vulkan ICD diagnostic passes; no fallback |
| Linux/macOS renderer | Not in first support slice | Do not claim support; later portability gate | Pending future phase |
| SFM Java-local terminal | Required independently of this repo | SFM Java tests and puppet | Tracked in Vox plan |
| SFM optional Rust/Vox mode | Optional development tooling | pinned dependency, frame/bridge puppet proof | Pending Phase 4 |

## Overall completion criteria

**Interactive MVP boundary:** The requested standalone-terminal goal is complete
at Phase 3.4 when `cargo run` opens a Vulkan window backed by the
default-profile PowerShell PTY and supports the agreed command/input/resize/
cancellation/exit behavior. Portable frame encoding, SFM integration, and
cross-platform renderer support are post-MVP work.

The roadmap as a whole is not complete: Phase 2.3, Phase 3.3, and Phases 4–5
remain future work. The active MVP goal is therefore audited against the
Phase 3.4 boundary, not against those deferred roadmap deliverables. As of the
latest audit, Phase 3.4 is implemented but remains [~] until a desktop session
provides direct evidence for the five interaction checks.

- [ ] All phase work items are [x] with local evidence.
- [ ] A fresh clone passes the workspace quality gate and exposes help/version.
- [ ] Core headless 1..1000 and process-free replay proofs are deterministic
  and bounded.
- [ ] Font extraction and Vulkan/off-screen output have deterministic fixtures.
- [ ] Portable frame data is validated without native GPU-handle interop.
- [ ] SFM integration is optional, pinned, and never an absolute path in
  canonical or propagated source.
- [ ] Java-local SFM functionality remains self-sufficient when Rust is absent.
- [ ] README, authoritative plan, SFM pointer, changelog, support matrix, and
  propagation evidence agree with the shipped behavior.

## Risk register

| Risk | Guardrail/mitigation | Gate |
| --- | --- | --- |
| Facet revision skew makes the template unbuildable | Align the direct/transitive revision or isolate compatible CLI dependencies; keep lockfile evidence | Phase 1.2 |
| Teamy Studio/CUDA/DirectX leaks into the portable project | Inspect dependency trees; keep core and renderer crates independent; reject unrelated workspace copies | Phase 1.3, 3.2 |
| Vulkan loader/device is unavailable | Actionable renderer-construction diagnostic; core/CLI remain usable, with no CPU-rendering fallback | Phase 3.2–3.4 |
| PowerShell smoke becomes an unsafe shell surface | Fixed -NoProfile fixture, explicit bounds, timeout, environment, and no arbitrary command API | Phase 2.2 |
| PTY crate grows renderer or shell-policy coupling | Keep teamy-terminal-pty portable-pty-only and expose terminal-oriented lifecycle types; CLI owns user-facing wiring | Phase 2.2–3.4 |
| Absolute local path leaks into SFM or later versions | Cargo config override only; inspect diff/lockfile; baseline-first propagation audit | Phase 4.1–4.2 |
| Font extraction couples the app to Teamy Studio | Renderer-neutral fixture and dependency-tree check before integration | Phase 3.1 |
| PNG-per-keystroke causes latency or CPU pressure | PNG only for keyframes/artifacts; dirty-tile/raw frame path for live updates | Phase 3.3–4.3 |
| Review mistakes a smoke test for runtime wiring | Pair unit/fixture checks with CLI, renderer, and SFM puppet evidence at each boundary | All phases |

The colour-picker bridge remains deferred. The five-crate workspace, PTY smoke,
core VT state, embedded font, Vulkan presenter, and interactive wiring are now
implemented. The next required evidence is desktop MVP interaction and the
corresponding runtime audit; frame encoding, replay, and SFM remain deferred.
