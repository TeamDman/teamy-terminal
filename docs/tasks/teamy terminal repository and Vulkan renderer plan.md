# Teamy Terminal — Resumable Implementation Plan

**Plan status:** Active
**Primary implementation root:** G:\Programming\Repos\teamy-terminal (main)
**Plan authority:** This file on teamy-terminal/main
**SFM pointer:** D:\Repos\Minecraft\SFM\repos2\1.19.2 (1.19.2)
**Last updated:** 2026-08-03

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

**Next slice:** 3.6.2b is complete: the retained CPU/full-PNG baseline now uses
bounded Vox `Tx`/`Rx` push delivery with no steady snapshot poller. Proceed to
3.6.3 by preserving and naming the CPU reference renderer/transport variants
before comparing GPU/slug and Java rendering candidates. Keep renderer and
transport selection independent, retain the completed correlated witness, and
do not infer that GPU glyph work wins until readback, encoding, transport, Java
upload, scheduling, and presentation are measured end to end.

Do not assume that CPU `fontdue` rasterization is the dominant bridged cost
merely because it is a plausible hypothesis: the Rust glyph pass,
PNG/readback/encoding, Vox transfer, Java PNG decode, Minecraft texture
allocation/upload, render-thread scheduling, and final presentation must be
independently observable. The code-level input mapping,
PTY/Vulkan startup, resize/reflow, large-output responsiveness, profiling
contract, portable frame encoding, visible-grid selection, and mouse selection
are complete. The elevated live control-plane gate now
covers prompt/command input, ANSI output, deterministic paste, resize, Ctrl+C
interruption, Ctrl+A, Ctrl+Backspace, Ctrl+Arrow, Ctrl+Shift navigation,
alternate-screen restoration, and clean shell close. Selection, copy, zoom,
and visibility can now be driven through typed `invoke` actions without desktop
input; the native wheel-direction mapping has bounded unit coverage. Prompt/command-range metadata is now covered by OSC 133 parsing and
interactive PowerShell shell integration; it still needs bridge-consumer use
in the later SFM slice. The control plane now also exposes
`invoke get-content`, a bounded visible-grid text probe that complements
`invoke screenshot` for deterministic desktop checks. `invoke select`,
`invoke copy-selection`, `invoke zoom`, `invoke show`, and `invoke hide` now
cover the corresponding app-owned behavior; `--hidden` creates a Vulkan-backed
window without initially presenting it or adding a taskbar tab.
The first Phase 4 bridge slice is now defined as Rust-authoritative terminal
state plus bounded full-PNG snapshots: Rust owns the PTY, VT parsing, colors,
scrollback, and rasterization; Java sends input/resize and displays the PNG.
Dirty-cell/tile and GPU-sharing optimizations remain later comparison work,
but retaining reusable CPU/Java resources and replacing polling with bounded
push delivery are immediate repairs justified by the completed end-to-end
witness.
SFM manual testing now adds a presentation constraint: at high Minecraft GUI
scale, the Java panel must not stretch a smaller Rust PNG. The bridge contract
must carry logical panel dimensions and cell metrics so Rust increases font
size while preserving the intended cell grid. The current CPU `fontdue` path
remains the correctness baseline; slug/GPU rasterization and persistent-target
optimization stay later slices.
The interactive follow-up now uses the existing Vox `send_key` and `send_mouse`
operations rather than buffering a Java command line: printable code points are
sent as exact text, control/navigation keys are sent as key transitions, and
mouse reports are encoded by Rust only when the terminal requests mouse mode.
The measured baseline has SFM poll `snapshot(after_sequence)` every 50 ms. That
proved Rust output can advance during a long-running command without another
Java input event, but it is not the target architecture: 3.6.2b replaces the
poller with a request-scoped Vox channel that pushes sequence-bearing frames
from Rust as soon as terminal state changes. Triple Esc closes the hosted panel
and triple Tab returns the event to Minecraft's normal focus traversal; both
sequences use a 1.5-second window and single/double presses remain terminal
input.
**Automation rendering decision:** `--hidden` is the supported near-term
background mode. It still creates a native winit window, Vulkan surface, and
swapchain so the production renderer path remains identical, but the window is
not presented or registered in the taskbar until `invoke show`. This is not
true headless GPU/Vulkan rendering: `VulkanRenderer` still requires a window
surface, while `OffscreenFrame` and the `fontdue` terminal rasterization path
are CPU-visible deterministic seams. A future no-window Vulkan device/image
renderer remains a separate post-MVP slice.
The `send-keys` grammar is intentionally explicit: every token is
`down:<key>`, `up:<key>`, or `wait:<milliseconds>`. There are no implicit
chords, delays, or text/numeric-key reinterpretations; the action keywords are
exact lowercase spellings, and raw text belongs to `send-text`. Outstanding
physical modifiers are released at the end of every request, including error
cleanup, and numeric keys use the same explicit `down:1`/`up:1` form as every
other physical key.

The first profiling evidence pass is complete enough to make the CPU damage
decision, not the final cross-process renderer decision: retain the full-frame baseline, and validate the opt-in
`dirty_rendering` prototype against it. The capture observed a low
changed-cell ratio while the current path still rasterized and uploaded the
whole frame; a naive changed-cell loop without cached frame storage and
swapchain-safe partial upload is not an acceptable optimization. The candidate
now has a successful 40-second GUI capture with generated output and explicit
800x450/1600x900 resizes; the stale registry entry from an earlier instance
remains visible but does not prevent new windows from launching. Those
standalone Rust captures do not include PNG transport or Minecraft's decode,
texture, scheduling, and presentation costs, so they cannot establish the
cause of the current SFM lag by themselves.

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
  teamy-terminal-frame, teamy-terminal-pty, teamy-terminal-renderer, and
  teamy-terminal-cli.
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

## Phase 1 — Workspace foundation [x]

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

**Completion notes:** The original synchronized fork was committed and pushed
as `23ad4dcd0`, but the terminal contract was subsequently added at
`aa75598dabb2138b18365cdf0d97ca94a34c5319`. The primary workspace now pins its
direct Facet/Figue/spec-proto/Vox family to that contract revision. The current
published `teamy-cancellation` feature set still names the older revision, so
the CLI does not enable its optional Figue integration; it keeps the equivalent
small `StopAfterArgs` adapter locally while using teamy-cancellation for the
runtime cancellation primitives. This leaves one `weavy` provider in the
primary lockfile without changing the cancellation repository.

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

## Phase 2 — Terminal core and headless proof [x]

### [x] 2.1 Define the terminal state and input contract

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
cursor/style transitions, bounded scrollback, resize, cursor reports,
visible-grid selection, bounded selected-text extraction, selection clearing
on resize/alternate-screen transitions, and deterministic snapshots. The CLI
adds bounded mouse selection, visible selection highlighting, selection-aware
damage invalidation, and Ctrl+C/Ctrl+Shift+C clipboard copy; Ctrl+C remains
the PTY interrupt path when no selection is active. OSC 133 A/B/C/D markers
publish bounded prompt and command ranges plus the reported exit status, and
interactive PowerShell startup emits the same markers while deterministic
`-NoProfile` tests remain unchanged. Process-free transcript replay is
covered separately by Phase 2.3. The core, CLI, and PTY focused suites pass
with 14, 14, and 4 tests respectively, including the OSC 133, selection, and
ConPTY input-mode boundaries.

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

### [x] 2.3 Replay the proof without launching PowerShell

**Work:** Persist a deterministic transcript/snapshot fixture and replay it
through the same core API. Include cancellation, process exit, malformed
output, and scrollback-limit cases.

**Validation:**

~~~powershell
cargo test -p teamy-terminal-cli replay -- --nocapture
~~~

**Completion criteria:** Replay reproduces visible rows/cells, cursor,
scrollback, and the fixture hash without a process, and bounded lifecycle and
malformed-output cases are asserted. Damage-region validation remains in the
renderer/CLI track.

**Completion notes:** Added the process-free replay runner in
`crates/teamy-terminal-cli/tests/replay.rs` and four independent transcript
fixtures covering ANSI styling, alternate-screen restoration,
resize/scrollback reflow, and malformed output with process-exit/cancellation
markers. Each fixture checks dimensions, cursor state, visibility, scrollback,
and a full-snapshot FNV-1a hash. `cargo test -p teamy-terminal-cli --test
replay --locked --offline` passes 1/1.

## Phase 3 — Font, Vulkan, and comparative rendering tracks [~]

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
inverse handling; its render test passes independently. `fontdue` is CPU
rasterization: it produces CPU-owned RGBA pixels, while Vulkan only uploads and
presents those completed pixels. A future glyph/rasterization path, such as the
slug algorithm work from Teamy Studio, is outside this handoff.

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

### [x] 3.3 Add off-screen readback and portable frame encoding

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

**Completion notes:** Added the renderer-free `teamy-terminal-frame` crate with
bounded RGBA8/BGRA8 metadata, sequence numbers, full-frame/dirty-tile kinds,
raw and PNG payloads, and strict dimension/stride/byte validation. The
renderer exposes `OffscreenFrame` as the CPU-visible readback seam without
leaking a native window or GPU handle. Frame and renderer tests cover raw
metadata, tight dirty-tile packing, PNG validation, malformed bounds, and
independent frame dimensions. The CLI command
`cargo run -p teamy-terminal-cli -- render-fixture --format png` writes a
deterministic 400x228 PNG fixture; the generated artifact was visually checked
and its packet metadata reports sequence 1, RGBA8 stride 1600, and 6351 bytes.

### [x] 3.4 Wire the interactive `cargo run` MVP

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
ANSI output, clipboard paste, resize, Ctrl+C interruption with no selection,
visible selection drag, Ctrl+A, Ctrl+Backspace, Ctrl+Arrow navigation,
Ctrl+Shift navigation, Ctrl+wheel font scaling, alternate-screen
entry/restore using `ratatui-key-debug`, and clean shell exit. Deterministic
selection/copy/zoom/visibility checks use the `invoke` control plane; a native
Ctrl+wheel session remains a useful UX smoke but is not required for the
app-owned behavior gate.

**Completion criteria:** `cargo run` opens a usable Vulkan terminal window and
the user can run ordinary shell commands through the default-profile
PowerShell session. PTY output, keyboard input, resize, cancellation, and
process exit are connected end to end. Missing Vulkan or another graphics
construction failure terminates with a diagnostic and no fallback renderer.

**Completion notes:** The CLI now owns the winit application handler and wires
PTY output, VT state, embedded-font frames, Vulkan presentation, keyboard text
and navigation, Ctrl+C, clipboard paste/copy, bounded mouse selection, resize,
cancellation, and child cleanup. Help/version continue to bypass interactive
launch. Pure input and selection tests cover text, Enter, navigation,
control-byte routing, bounded mouse coordinates, selection damage, and
clipboard behavior; the bounded `-NoProfile` PTY smoke covers command input
and successful exit. The core now records ConPTY's `?9001h/l` Win32-input
mode request, and mode-aware keyboard/control encoding emits the documented
`KEY_EVENT_RECORD` sequences while retaining raw paste. A focused interactive
PTY regression, including DSR cursor-report responses and an initial resize,
is retained as an ignored desktop-account gate for Ctrl+C against `ping.exe -t`
using the exact generic two-record sequence emitted by the manual winit path.
It cannot run meaningfully under the Codex sandbox identity
(`teamy-desktop-w\codexsandboxoffline`), which accepts the input bytes but
cannot deliver the resulting ConPTY console-control event; the elevated
desktop run passes with `cargo test -p teamy-terminal-pty --lib -- --ignored`.
`send-text` uses the same Win32 key-record encoding for text and Enter
when that mode is active, while paste remains raw. A timing-aware elevated
control-plane probe now passes: `invoke get-content` observes the running ping,
the explicit `down:/wait:/up:` Ctrl+C sequence produces `Control-C` and a fresh
prompt, and `invoke close` returns `Ok`. Closing also has a typed `windows-rs`
fallback that enumerates the PTY process tree and terminates it through typed
Win32 APIs; the fallback treats an already-exited child as successful.

The same timing-aware desktop probe also passed deterministic `invoke paste`
input, Ctrl+A as the profile's PSReadLine `SelectAll` binding, Ctrl+Backspace,
Ctrl+Left/Right, Ctrl+Shift+Left replacement, and an 800x450 resize whose PNG
IHDR matched the requested dimensions. The navigation wire aliases now accept
the hyphenated names emitted by the control client (`arrow-left`, `page-up`,
and peers), with a dedicated round-trip test. Launching
`ratatui-key-debug` and sending its documented three-Esc exit sequence restored
the pre-alternate-screen marker and then closed cleanly. The accepted native
mouse probe used focused normalized Win32 coordinates and highlighted
`SELECTION_MARKER`; the remaining clipboard/zoom evidence is now covered by
the app-owned control plane. A hidden Vulkan launch was validated with
selection, clipboard copy, zoom, screenshot, and hide/show/hide actions; the
copied response was exactly `SELECTION_MARKER`.

The follow-up control-plane slice adds an intentionally literal
`down:/up:/wait:` grammar, numeric-key coverage, manual-compatible generic
Win32 key records with state-only modifiers, and guaranteed modifier cleanup.
Its parser/unit tests pass. The direct PTY interrupt regression remains
ignored under the sandbox for the same desktop-account reason, while both the
elevated PTY regression and the live Windows control-plane interrupt probe are
green. The Phase 3.4 gate is closed by the typed hidden-window probe and the
wheel mapping tests; native Ctrl+wheel remains a manual UX smoke. The earlier
staged OpenConsole/`conpty.dll` retry
was removed and is not treated as a solution.

Interactive polish now maps Ctrl+Backspace to the backward-kill-word control
byte used by PowerShell-compatible prompts, uses a black default terminal
background so gray and white text remain visibly distinct, and keeps the
existing ANSI palette values explicit and tested. Ctrl+wheel font scaling is
bounded to 8–32 physical pixels in 1-pixel steps; each change rebuilds the
font metrics/cache, invalidates the cached frame, reflows the terminal, and
resizes the PTY. Automated direction/bounds tests pass, and the same behavior
is exercised through `invoke zoom` on a hidden Vulkan-backed instance.

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

### [x] 3.5.2 Make Tracy and tracing-subscriber feature selection explicit

**Completion notes:** The Teamy Terminal profiling feature contract is complete
for the current handoff scope. The CLI exposes `extended_observability` for
detailed spans and `tracy` as the transport feature, with explicit subscriber
capabilities and profiling-only log filtering. The current teamy-profiler
dry-run and live-capture contract are recorded below.

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

The Teamy Terminal and teamy-rust-cli feature graphs no longer include
`tracing-subscriber` default features through teamy-cancellation. The dedicated
teamy-mft repair branch is also clean. Cloud Terrastodon's older remote
teamy-mft chain remains deliberately outside this handoff; no new Cloud
changes are required for the SFM integration.

#### [x] 3.5.2.a Repair teamy-cancellation's tracing feature boundary

**Work:** Update the local `G:\Programming\Repos\teamy-cancellation`
checkout so its optional `tracing-subscriber` dependency uses
`default-features = false` and an explicit `registry` capability, which is the
only subscriber capability used by `StopAfterLayer`. Keep its Facet/Figue pin
at the compatible `23ad4dcd0` revision. Add or retain a focused feature-graph
check and run the crate's all-feature tests. After the repair is reviewed,
propagate the resulting teamy-cancellation revision (or a release containing
it) to teamy-rust-cli and teamy-terminal; do not leave those consumers on mixed
Facet or tracing-subscriber contracts. Cloud Terrastodon is explicitly
excluded from this handoff.

**Validation:** From teamy-cancellation, run `cargo tree -e features -i
tracing-subscriber`, `cargo test --all-features --locked`, and the repository
quality gate. Then rerun the same feature-graph audit and locked focused
checks in each consumer, including the template's Facet-bound compile and
the teamy-profiler dry-runs.

**Completion criteria:** No in-scope consumer's resolved dependency graph
reaches `tracing-subscriber feature "default"` through teamy-cancellation; the
template compiles without duplicate incompatible Facet traits; all updated
pins are recorded in lockfiles; and the existing profiling manifests remain
unchanged apart from their source revision metadata. Cloud propagation remains
an explicit later task.

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

#### [x] 3.5.2.c Audit and lock the application profiling feature graph

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

**Completion notes:** Teamy Terminal's default, Tracy, and
extended-observability builds were checked with isolated target directories.
The application graph uses only the explicitly selected subscriber and Tracy
capabilities.

### [x] 3.5.3 Instrument the performance-critical terminal path

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
captures are now complete. Further persistent-render-target or partial-upload
investigation is deferred; it must not weaken the stale-image full-refresh rule
and is not a pre-SFM requirement. The accepted performance boundary for this
plan is CPU dirty-cell rasterization with correctness-first full-upload
fallback when the rotating MAILBOX image is stale. Do not accept a cell-diff-
only change that still clears/rasterizes/uploads the full frame.

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

### [x] 3.5.4 Synchronize the template and profiler source of truth

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

### [x] 3.5.6 Couple external frame size to logical cells and font metrics

**Work:** Extend the portable frame/bridge contract so an SFM panel provides
logical dimensions and terminal cell metrics, and the Rust renderer selects a
font size that keeps the requested cell grid stable. Java must present the
result without bitmap upscaling. Keep the CPU `fontdue` renderer as the
reference implementation and leave slug/GPU rasterization and persistent
swapchain targets to later optimization work.

**Validation:** Capture the same terminal content at normal Minecraft GUI scale
and GUI scale 7. Assert the same requested `columns × rows`, larger Rust
font/cell pixel dimensions, and a native-sized delivered frame through typed
content/frame-metadata/screenshot witnesses. Compare it against a
Java-stretched-frame negative control and add a bridge fixture for resize and
stale-frame rejection.

**Completion criteria:** High-scale SFM presentation remains crisp enough to
prove that font size, not post-render bitmap stretching, is responding to the
available panel size; the logical-size contract is documented and covered by
focused tests; and no GPU/slug implementation is required to pass this gate.

**Completion notes — 2026-08-02:** Facet/Vox commit `b59d4a1a6` carries the
logical grid, physical panel target, cell metrics, font pixel size, and snapshot
timings through the generated contract; Teamy Terminal commit `cc1409a`
populates those fields on the CPU/full-PNG path. The normal-scale SFM puppet
passed with native `425×238` frames, while the supported high-scale run at
`3840×2130@7` reached effective GUI scale 7 and produced crisp native frames
with `547×303` panel metrics, `91×27` logical cells, and `7×13` cells. The
`1280×720@7` probe is intentionally recorded as a clamp diagnostic because
Minecraft limits that viewport to GUI scale 3; the high-scale screenshot path
does not rely on stretching a smaller PNG. The 4K full puppet later exceeded
the harness action budget during its final capture, but the scale-7 terminal
captures and metadata completed before that timeout. No GPU/slug work is
required for this gate.

## Phase 3.6 — Cross-renderer evidence laboratory and GPU text path [ ]

This phase turns terminal performance into a reproducible comparison rather
than a sequence of renderer guesses. The motivating symptom is the user's
current perception of a multi-second delay in the Rust-backed SFM terminal.
The first gate is to observe that delay computationally on the current CPU
full-PNG behavior. The CPU `fontdue` path is a hypothesis and the stable visual
reference, not a preselected culprit or loser. A GPU path is not considered a
GPU glyph renderer if it merely uploads pixels already rasterized by
`fontdue` on the CPU.

The comparison must keep terminal semantics independent from presentation.
One terminal snapshot and damage stream must be consumable by Rust CPU, Rust
GPU/slug, and Java cell renderers. Pixel transport and semantic-cell transport
are separate variables: a fast Rust GPU rasterizer may still lose to GPU
readback, PNG encoding, transfer, Java decoding, or texture upload, while a
Java semantic-cell renderer may save bandwidth but spend more time in
Minecraft font/layout calls. Measure the complete path and every major stage.

### [x] 3.6.1 Capture the perceived multi-second bridge delay

**Work:** Add a cross-process correlation id and terminal/frame sequence to the
current Rust CPU full-PNG path. Record the local monotonic timestamps or
durations for input receipt, PTY drain, VT mutation, snapshot/damage creation,
glyph rasterization, frame construction, GPU upload/present where applicable,
GPU readback, PNG/raw encoding, response send, Java receive, Java decode,
texture allocation/update/upload, panel render, and the first frame that
reports the target sequence as presented. Do not subtract unrelated Rust and
Java clocks as though they were synchronized; transmit Rust stage durations
and let Java measure its own stages plus end-to-end request/output-to-present
latency.

Exercise cold and warm glyph caches, idle/prompt typing, `1..100`,
`1..10000 | Out-Host`, cyan ANSI output, scroll floods, alternate-screen
entry/restoration, panel resize, normal GUI scale, GUI scale 7, and a split
panel. Use repeated samples and retain the exact user-visible scenario that
reproduces the lag. If unattended automation cannot reproduce the perceived
multi-second magnitude, expose typed start/mark/stop capture actions so a
manual session can produce the same manifest without relying on stopwatch
memory or screen recording.

**Validation:** A checked manifest identifies source revisions, CPU/GPU,
driver, OS, Java/Minecraft version, window/panel dimensions, GUI scale,
columns/rows, font and size, renderer, transport, cache state, workload,
sample count, bytes, dropped/stale frames, and stage timings. The current
default path has a machine-readable input-or-output-to-present measurement in
the same multi-second range perceived by the user, or the plan records which
tested scenarios failed to reproduce it and the manual capture needed to do
so.

**Completion criteria:** We can point to the correlated sequence and stage
durations that account for the visible delay. No backend is promoted and no
GPU performance claim is accepted before this witness exists.

**Completion notes — 2026-08-02:** Teamy Terminal commit `cc1409a` emits the
snapshot timing fields from the real PowerShell Vox server, and the refreshed
Facet/Vox artifact is Facet commit `b59d4a1a6` with SFM lock hash
`blake3:d5c94d8fd6fad6671d5f7597180aadea4b7fec10`. The focused Rust witness
measured the cold `1200×760 / 120×40` sample at `2,853,889 µs`, including
`font_load_us=2,278,390`, `raster_us=136,599`, and
`png_encode_us=393,669`. A clean headless SFM puppet then passed at normal
scale with correlated `SFM_VOX_TERMINAL_TIMING` and
`SFM_TERMINAL_PRESENTATION_TIMING` records. The live records show warm Rust
totals around `0.75–1.1 s`, font loading around `0.7–1.0 s`, raster/encode in
the low-millisecond range, Java PNG decode in sub-millisecond samples, Java
texture allocation/registration also sub-millisecond, and Java render/present
work in the low-millisecond range. Java wait/poll time is recorded separately;
no unsynchronized Rust/Java clocks are subtracted. The machine-readable
content witnesses and screenshots are under
`platform/minecraft/runGameTestPreview/terminal-content/` and
`platform/minecraft/runGameTestPreview/screenshots/`, with the full correlated
console log at `platform/minecraft/build/sfm-toolchain/run/runGameTestPreview/console.log`.
This closes the current-path lag gate without selecting a renderer or claiming
that all future workload/transport matrix samples are complete.

### [~] 3.6.2 Freeze renderer-neutral snapshot, request, and result contracts

**Work:** Keep PTY ownership, VT parsing, scrollback, cursor, selection, color,
style, wide-cell occupancy, and damage calculation in the renderer-free core.
Define a bounded render request containing session/sequence, logical
columns/rows, physical pixel extent, requested cell/font metrics, font
selection, complete-versus-damage state, cursor/selection overlays, and cache
invalidation reason. Define a render result containing backend identity,
native pixel dimensions, cell metrics, full-frame/dirty-region metadata,
optional portable pixels or renderer-owned presentation data, visual hash,
and bounded stage statistics.

Semantic transport must carry terminal cells, not plain transcript text. It
must preserve grapheme/code point, foreground/background, bold, underline,
inverse, width/continuation cells, cursor shape/location, selection, damage,
sequence, and full-refresh semantics. Renderer and transport selection must
be explicit in CLI/configuration and artifact manifests so changing one does
not silently change the other.

**Validation:** The same deterministic snapshots and damage sequences are
accepted by multiple test backends. Bounds, stale/out-of-order sequences,
malformed continuation cells, resize invalidation, and unsupported capability
negotiation have focused tests. Core/font crates do not acquire winit, Vulkan,
Minecraft, or Vox dependencies.

**Completion criteria:** Rust CPU, Rust GPU/slug, and imported Java comparison
results can share one fixture identity and one semantic correctness oracle
without sharing application-specific renderer objects.

**Progress notes — 2026-08-02:** The initial reviewed contract is implemented
and generated in Facet/Vox commit `b59d4a1a6`. It adds bounded
`TerminalSurfaceMetrics`, explicit backend/transport identifiers, request and
frame correlation IDs, native frame dimensions/cell/font metrics, snapshot
timing metadata, and Java compatibility constructors for newly defaulted
fields. Facet protocol tests, Java conformance/smoke/package tests, the SFM
compile, and the live CPU/full-PNG bridge proof pass. The broader
multi-backend semantic-cell/damage contract and Rust GPU/slug fixture identity
remain intentionally open; this initial slice does not claim those renderers.

### [x] 3.6.2a Retain reusable font, glyph, frame, and encoding resources

**Work:** Remove per-snapshot resource reconstruction from the CPU/full-PNG
reference before comparing it with other renderers. Split immutable font-face
discovery and byte loading from size-specific raster state. Scan the system
font database and resolve the primary/fallback face set once per process (or
once per immutable font identity), then retain a bounded renderer cache keyed
by the exact font identity/hash, fallback set, backend, and effective pixel
size. A repeated frame at the same size must reuse the same `TerminalFont`,
`ScaleContext`, and per-glyph bitmaps. A resize from A to B and back to A must
reuse A while it remains inside the documented bounded cache; resize may
invalidate layout/frame contents without discarding reusable font faces or
glyphs.

Retain and resize CPU frame storage and PNG output scratch capacity rather
than allocating fresh backing vectors for every frame. Keep renderer state on
the terminal session (or in an explicitly bounded shared renderer pool), and
do not hold the global session-map lock while rasterizing or encoding. Record
constructor/cache-hit/cache-miss/eviction counters and separate one-time font
discovery, size-renderer creation, new-glyph rasterization, cell composition,
and PNG encoding durations. On the SFM side, retain one registered
`DynamicTexture` and its reusable upload storage while dimensions are
compatible; replace and close it only when dimensions/capabilities require a
new allocation. If PNG decoding necessarily creates a temporary
`NativeImage`, measure and dispose that temporary explicitly rather than
mistaking texture reuse for decode-buffer reuse.

**Discovery notes — 2026-08-03:** The measured delay is an object-lifetime
bug, not a one-second frame timer. `VoxTerminalService::encode_png` currently
constructs `TerminalFont::new(DEFAULT_PIXEL_SIZE)` for every non-empty
snapshot and can construct a second font for a scaled panel. Each constructor
rescans system fonts, starts with an empty glyph `HashMap`, and both objects
are discarded after the PNG is returned. This accounts for roughly
`0.7–1.0 s` of every warm live frame in the existing witness. The frame and
PNG vectors are likewise newly allocated on that path. Java also decodes,
allocates, and registers a new dynamic texture on sequence changes even when
the dimensions are unchanged; its measured cost is smaller but its lifetime
is still needlessly tied to a frame.

**Validation:** Add focused constructor/cache tests and counters proving that
two same-size renders perform one font-face discovery and one size-renderer
construction, that repeated characters hit retained glyphs, and that A→B→A
resize reuses A without stale pixels or metrics. Exercise multiple sessions
and enough sizes to prove the cache bound and deterministic eviction. Re-run
the Rust timing witness and the real SFM puppet with cold and warm manifests;
after the first construction, unchanged-size frames must report no font-face
discovery or renderer reconstruction, while visual/content fixtures remain
equivalent. Java telemetry must show texture registration/allocation does not
grow once per frame at stable dimensions and that every replaced resource is
closed exactly once.

**Completion criteria:** Repeated changed frames at a stable size retain the
font/glyph renderer and compatible frame/texture storage; reversible resizes
reuse bounded cached state; the warm correlated witness no longer attributes
hundreds of milliseconds to font loading; and no cache is unbounded, stale,
or silently shared across incompatible font/size/backend identities.

**Completion evidence — 2026-08-03:** Teamy Terminal commit `1c5fd0d` splits
process-wide immutable font discovery/faces from size-specific renderers,
retains a four-entry per-session size LRU and 4,096-entry per-renderer glyph
LRU, reuses frame and PNG backing storage, and drops the global session-map
lock before render/encode work. The focused warm witness reports one process
catalog load, one renderer construction, nine renderer-cache hits, 70 glyph
hits versus 19 initial misses, retained frame/PNG buffers, and
`font_load_us=0` on request sequence 7. The font tests pass 8/8 and the Vox
server tests pass 9/9, including stable size, bounded eviction, multiple
sessions, and A→B→A reuse. The installed executable identifies the exact clean
revision `1c5fd0d`.

SFM commits `ce8c53f44`, `343f10f72`, and `4c2846618` retain and instrument
the compatible Minecraft texture/upload path and make the restart witness
deterministic. The passing normal puppet run
`sfm-title_screen-20260803-112610-670` contains 139 correlated presentation
samples: 137 warm samples report zero font-load time, the two expected cold
samples correspond to the initial and deliberately restarted Rust processes,
and 139 uploads use only two texture allocations/registrations (the one
replacement is the deliberate resize) with 137 in-place reuses. The passing
GUI-scale-7 run `sfm-title_screen-20260803-113254-252` reaches effective scale
7 and a native 547×303 panel, completes all 13 visual captures and content
assertions, and records 134 uploads with two registrations and 132 reuses;
132 of 134 correlated samples report `rust_font_load_us=0`, and the final warm
raster/encode stages are sub-millisecond/about two milliseconds rather than
the former recurring 0.7–1.0-second font load. PNG decoding still requires a
measured, explicitly closed temporary `NativeImage`; that unavoidable
full-PNG cost remains visible for later transport comparisons. A full
workspace Rust test attempt also exposed only the unrelated pre-existing
stochastic `fuzz_cli_args_roundtrip` zero-valid-sample failure; all 23 library
tests and every focused resource/lifecycle test passed.

### [x] 3.6.2b Replace live snapshot polling with bounded Vox Tx/Rx push

**Work:** Extend the generated Facet/Vox terminal service with a typed
request-scoped frame subscription, expected to have the directional shape
`subscribe_frames(request, frames: Tx<TerminalFrameEvent>)`: the Java caller
creates the channel pair, passes `Tx` to the server call, and consumes the
paired `Rx`; the Rust handler owns the bound `Tx` and sends toward Java. The
event carries session/connection epoch, monotonically increasing terminal and
frame sequences, correlation id, full-versus-damage metadata, negotiated
bounds/backend/transport, frame payload, and local stage timings. The initial
event after subscribe/re-subscribe is a complete latest-state frame. Raw Vox
channels are request-scoped rather than durable: disconnect closes the call,
reconnect opens a fresh subscription, and Rust sends a full resynchronization
frame instead of claiming transparent stream resume.

Facet's current Java runtime explicitly rejects non-empty request-channel
metadata as outside its unary slice. Implement and test credit-controlled Java
`Tx`/`Rx` runtime support plus generated terminal client bindings first, then
package and pin that reviewed Facet revision before changing SFM. Preserve the
unary `snapshot(after_sequence)` method for explicit screenshots, diagnostics,
bootstrap compatibility, and recovery checks, but remove it from the normal
live-frame loop; final SFM production code must have no
`FRAME_POLL_MILLIS`-style frame scheduler.

Refactor each Rust terminal session so PTY events are drained continuously by
the session owner rather than opportunistically only when unary calls arrive.
Every VT mutation advances the authoritative sequence and immediately marks
the newest state ready for publication. Do not add a debounce/minimum frame
interval. Bound work with one render/send in flight and one newest pending
sequence: if output changes while a frame is rendering or blocked on Vox
credit, supersede intermediate pending states and render the latest state as
soon as the in-flight operation completes. Never queue one full PNG per PTY
chunk. Resize/input/cancellation/alternate-screen transitions use the same
sequence and publication path. Separate the global session registry from
per-session mutation/render synchronization so one slow client/session cannot
block unrelated sessions.

On Java, consume the `Rx` on a dedicated connection/receiver executor, reject
wrong-epoch and non-monotonic frames, retain only the newest acceptable frame
awaiting render-thread presentation, and schedule Minecraft work without
blocking the Vox driver. Channel closure or connection replacement clears the
old epoch and follows the existing disconnected/retry UI. Keep bounded
counters for mutations, renders started/completed, pre-render coalescing,
credit-blocked sends, pushed/received/superseded/presented frames, queue depth,
and output-change-to-first-present latency so the push conversion remains
comparable to the completed polling witness.

**Validation:** Add Facet Rust/Java generated round trips that exceed the
default channel credit, propagate close/drop/error correctly, and prove the
terminal event schema. Add a real endpoint test in which PowerShell emits
output after subscription without any further Java request: Java must receive
intermediate and final sequences, while instrumentation proves zero periodic
snapshot calls and no idle busy loop. Cover slow receiver/backpressure,
`1..10000`, Ctrl+C, resize, alternate screen, channel cancellation,
disconnect/reconnect with full resync, stale old-epoch delivery, and two
independent sessions. Compare p50/p95/max change-to-present latency and CPU
use against the 50 ms polling baseline after 3.6.2a is enabled.

**Completion criteria:** Live frames are delivered solely by the Vox channel,
begin publication immediately on Rust sequence mutation, remain bounded under
an output flood or stalled Java receiver, recover through an explicit fresh
subscription, and require no timer-driven Java frame polling. One-shot
snapshot remains functional but is absent from steady-state live telemetry.

**Completion evidence — 2026-08-03:** Teamy Terminal commit `7117b00` moves PTY
drain/publication into the session actor, publishes immediately through a typed
request-scoped channel, permits one render/send in flight plus one newest
pending sequence, reports credit/coalescing/queue counters, emits a complete
initial/resynchronization frame, and isolates session ownership. Eleven focused
producer tests pass, including slow-credit coalescing, cancellation/restart,
stale epochs, and independent sessions; seven PTY tests pass, with the one
pre-existing sandbox-only Ctrl+C case still ignored. Facet commits
`75833707780b8a4570638044b7714fe6030065f0` and
`4d62d2cfb211f48f89ca4a06c466ca8fe452ca03` provide the Java `Tx`/`Rx`
runtime/codegen/package and exact Rust-authoritative nested schema closures.
The Facet gates pass 43 vox-codegen tests, 78 Java assertions, 12 Java-wire
tests, formatting, and deterministic package verification; SFM pins the latter
revision and package hash `14d192241617c123a37b37434b7f0bfbf681eb3e`.

The normal and 3840×2130 GUI-scale-7 SFM puppets both pass the complete terminal
interaction/restart witness. They record zero steady snapshot calls and zero
poll loops, two subscriptions, one retired pre-restart channel, no rejected
events, producer pending-depth maximum one, and completed render/send counts.
Normal receives/accepts 397 events, delivers 393 Java frames, supersedes four,
and records 34 completed pushes with 119 pre-render coalesces. GUI-scale 7
receives/accepts 399 events, delivers 397 Java frames, supersedes two, and
records 32 completed pushes with 125 pre-render coalesces. The latter
mutation-to-send sample is 4,321 microseconds. Machine-readable evidence is
checked into SFM as
`docs/architecture/evidence/vox-terminal-push-delivery-1.19.2.json`.

### [ ] 3.6.3 Preserve and parameterize the Rust CPU reference paths

**Work:** Retain `rust-cpu-fontdue` as the explicit renderer identity, with
`full` and `dirty` raster/damage modes kept separate from transport selection.
Implement stable `full-png`, `full-raw-rgba`, and `dirty-raw-rgba` transport ids
for this slice. A losslessly compressed dirty transport is a later candidate,
not an alternative left undecided inside this item. Raw pixels use a bounded,
versioned RGBA8 contract with explicit origin, dimensions, row stride, alpha
interpretation, frame/base sequence, and ordered dirty-region coordinates;
malformed lengths, bounds, and base sequences are rejected. Keep Caskaydia Cove
Nerd Font Mono version/provenance, font size, fallback choice, and glyph-cache
state explicit. Measure persistent render targets and partial uploads separately
from glyph rasterization so an upload improvement is not reported as a font
improvement.

The subscription contract must expose stable transport ids and capabilities so
an SFM terminal panel can select its desired transport independently of other
terminal panels. A selection change keeps the Rust terminal session/PTY alive,
creates a fresh transport generation, and begins with a complete
resynchronization frame in the selected encoding. Frames from the retired
generation are stale even if their numeric sequence is higher. Distinguish the
requested transport from the transport proven active by an accepted frame;
unsupported combinations fail explicitly and never fall back silently.

**Validation:** Full-frame and dirty paths are pixel-equivalent for the same
fontdue request after composing dirty regions. The fixture suite covers ANSI
colors, bold, underline, inverse, cursor variants, selection, wide glyphs,
combining/fallback characters, scroll, alternate screen, resize, and forced
full refresh. Existing `--no-default-features` and dirty-rendering baselines
remain reproducible. A transport switch preserves the terminal session and
input stream, accepts a full frame in the newly selected encoding, rejects late
old-generation frames, and leaves another panel's independently selected
subscription unchanged.

**Completion criteria:** The CPU reference can be run headlessly and through
the real bridge with stable backend/transport names, comparable manifests,
and no accidental bitmap scaling.

### [ ] 3.6.4 Extract Teamy Studio's slug algorithm into a true GPU backend

**Work:** Audit the current Teamy Studio slug implementation and copy only the
font geometry/coverage, shader, atlas/buffer, and test concepts needed behind
the renderer-neutral request. Record source revision and file provenance.
Exclude Teamy Studio application/window models and unrelated DirectX, CUDA,
Burn, PyTorch, or UI dependencies. The new backend must generate glyph
coverage/compositing on the GPU; Vulkan presentation of CPU-fontdue RGBA bytes
does not satisfy this item.

The planning audit found the current reference at Teamy Studio revision
`c827d78c4b60b64348a82db9c7a961c01c8d0701`: the implementation and CPU
oracles live in
`crates/teamy_studio_shell/src/windows_d3d12_renderer.rs`, with design and
known band/glyph regressions in `docs/notes/slug-font-renderer.md` and
`docs/notes/text-rendering-glyphy-band-contract-plan.md`. Re-record the exact
revision used when extraction begins rather than assuming this planning pin
remains current.

Support damage, glyph caching, colors/styles, cursor/selection, wide cells,
resize, font-size changes, and deterministic readback. Add a true no-window
Vulkan device/image path for automated rendering if the platform supports it;
retain `--hidden` production-window execution as a separate comparison rather
than calling it headless. Because SFM cannot assume cross-process native GPU
handle sharing, measure GPU readback, encoding, and portable transfer costs
independently. Native-handle interop remains an optional later experiment.

**Validation:** CPU/fontdue and GPU/slug consume the same fixtures. GPU
readback artifacts are bounded and repeatable enough for visual comparison;
shader/geometry tests include the known difficult glyphs and band-contract
regressions documented by Teamy Studio. Missing Vulkan produces an actionable
skip/diagnostic without invalidating CPU/core tests.

**Completion criteria:** A selectable `rust-gpu-slug` backend renders terminal
glyphs on Vulkan, produces native-sized visible artifacts, and reports raster,
GPU execution/readback, encode, and transfer stages separately.

### [ ] 3.6.5 Produce visual comparisons across every renderer/font pair

**Work:** Build a deterministic comparison command that consumes one fixture
set plus imported SFM Java artifacts and emits the source snapshot, each PNG,
heat-map/diff images, and a self-contained HTML/JSON report. Compare at fixed
logical grids, native pixel extents, font sizes, GUI scales, and cold/warm
caches. Include Rust fontdue with Caskaydia Cove Nerd Font Mono, Rust GPU/slug
with that same face, Java with the vanilla Minecraft font, and Java with the
same named/versioned Caskaydia Cove Nerd Font Mono face used by Rust.

Use exact pixel equality only for repeat runs or backends expected to be
pixel-identical. Different rasterizers and fonts require semantic assertions
plus recorded geometry, color, glyph-occupancy, and perceptual/image-distance
metrics; an antialiasing difference must not hide a missing glyph, wrong cell,
wrong color, stale cursor, or bitmap stretch. Reports must make side-by-side
human inspection easy while remaining machine-readable.

**Validation:** Fixtures include ASCII prompt text, Nerd Font glyphs, fallback
Unicode, ANSI palette samples, bold/underline/inverse, cursor and selection,
wide/combining text, dense output, and normal/high GUI-scale captures. Every
artifact records renderer, transport, font identity/hash, cell metrics,
logical grid, and native image dimensions.

**Completion criteria:** A reviewer can compare visual quality and terminal
correctness without guessing which backend/font produced an image, and CI can
detect semantic or material visual regressions without demanding false
cross-font pixel identity.

### [ ] 3.6.6 Run the temporal matrix and make an evidence-based default decision

**Work:** Run matched repeated samples for each valid combination of renderer
(`rust-cpu-fontdue`, `rust-gpu-slug`, `java-vanilla`, `java-caskaydia`),
transport (full PNG, full raw pixels, dirty pixels/tiles, semantic cells),
workload, layout, GUI scale, and cache state. Invalid combinations must be
reported as unsupported rather than silently substituted. Record p50/p95/p99
and maximum input-to-present and output-complete-to-present latency, Rust and
Java CPU time, GPU time when available, rasterized cells/pixels, bytes encoded
and transferred, allocations, texture creations/updates, uploaded bytes,
frames rendered/presented/dropped/coalesced/stale, queue depth, and resize or
reflow time.

Use Teamy Profiler/Tracy to locate work and the fixed benchmark manifest as the
regression gate. Separate cold startup from steady-state interaction and
separate Rust GPU raster time from readback/encoding/bridge costs. Select the
default bridge renderer/transport from end-to-end evidence, correctness, and
visual quality; do not select GPU merely because one internal GPU span is
shorter. Retain the CPU reference and at least one semantic Java comparator
for diagnosis even if they are not the default.

**Validation:** The report reproduces the original lag witness, reruns it on
every candidate, and shows matched before/after distributions with source and
environment manifests. The selected path passes the full semantic fixture and
visual acceptance suite and has no unexplained multi-second outliers.

**Completion criteria:** The plan records the selected default, rejected or
deferred combinations and why, measured trade-offs, fallback policy, and exact
commands/artifacts required to repeat the decision.

### Parallel implementation topology

The lag witness and contract are integration foundations, but implementation
does not need to become serial after their first reviewed version. Assign
separate subagents or user-visible Codex tasks backed by isolated
branches/worktrees for each track, record
their exact locations here when work begins, and give one integration owner
responsibility for contract changes. No two agents should edit the same
canonical plan or generated Vox output concurrently.

| Track | May start after | Primary ownership | Parallel deliverable |
| --- | --- | --- | --- |
| P0 — correlation and benchmark contract | immediately | shared manifest, ids, stage vocabulary, lag reproducer | 3.6.1 plus the first reviewed 3.6.2 contract |
| P1a — retained CPU resources | completed 3.6.1 witness | teamy-terminal font/frame/CPU session state | 3.6.2a retained face/size/glyph/frame/encode caches and warm witness |
| P1b — Rust push producer | reviewed P5 channel contract; may prototype behind Rust-only tests earlier | teamy-terminal PTY/session/publication path | 3.6.2b immediate latest-state producer with bounded coalescing/backpressure |
| P1c — Rust CPU/transport baselines | P1a/P1b integrated | teamy-terminal font/frame/CPU paths | 3.6.3 full/dirty PNG/raw evidence |
| P2 — Rust GPU/slug | P0 render-request draft | isolated Teamy Studio audit and teamy-terminal Vulkan/slug backend | 3.6.4 GPU fixtures and timings |
| P3 — Java vanilla renderer | P0 semantic snapshot draft | canonical-oldest SFM Java panel backend | vanilla-font artifacts and Java timings |
| P4 — Java Caskaydia renderer | font identity and licensing frozen | SFM resources/font adapter, separate from P3 where practical | same-font Java artifacts and metrics |
| P5 — Vox channel and cell/damage transport | immediately for Java `Tx`/`Rx`; P0 semantic draft for cells/damage | isolated Facet/Vox runtime/codegen/contract worktree plus generated Java/Rust fixtures | credit-controlled Java channels, terminal frame subscription, then bounded semantic-cell capability |
| P5b — SFM push consumer and retained texture | reviewed/generated P5 frame subscription; texture work may start against a fake receiver | canonical-oldest SFM terminal service/presenter | no frame poller, latest-only Rx consumption, stable-dimension texture reuse, reconnect/full-resync proof |
| P6 — report and visual-diff tooling | fixture ids and manifest schema frozen | renderer-independent artifact/report tooling | HTML/JSON matrix, diffs, and comparison summaries |

P1a, the Facet Java-channel portion of P5, and P6 may start concurrently.
P1b and P5b integrate only after the reviewed generated subscription contract;
P2–P4 may continue in isolated work after their shared request/snapshot seams
are frozen. Integration imports immutable commits/artifacts from each track. P3/P4 must
follow SFM's oldest-branch-first rule and must not propagate until the selected
canonical implementation is accepted. P5 must not modify Cloud Terrastodon.
The final matrix and default decision in 3.6.6 are integration work and remain
sequential after the candidate evidence is available.

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

**Current boundary:** The reviewed Facet/Vox source-of-truth extension is
committed at `858e64cdc` on `teamy/vox-java-terminal-contract`, based on the
earlier reviewed boundaries `144382fd5` and `35461380d`. It carries frame
kind/stride/tile bounds, cursor, selection, mouse button state, capability
flags, and OSC 133 prompt/command metadata, with generated Java 17 sources.
Rust schema tests, codegen freshness, and the focused generator compile pass.
The SFM propagation CLI now has a `CargoCommand` source-build recipe for the
dependency-free `vox-java` artifact, including the locked `cargo run
--locked --package vox-xtask -- package-java` task and portable output path.
The exact contract commit `26fda8736` is still local-only and is not yet a
published or SFM-consumed pin; broad `test-java` remains environment-limited
by the installed JBR 17.0.6 `Cannot close compiler resources` failure in the
non-elevated environment, while the elevated package gate passes.

### [x] 4.3 Prove the Rust-authoritative full-PNG endpoint

**Work:** Keep the initial endpoint in the isolated
`tools/vox-terminal-server` workspace until its Facet revision can be aligned
with the main workspace. It owns the real `pwsh -NoProfile` PTY, terminal VT
state, colors, scrollback, and CPU PNG rasterization. It must answer terminal
device/cursor queries, bound input/output/frame sizes, and expose only the
generated Vox terminal contract.

**Validation:** Run the focused Rust puppet test with `1..100` and
`Write-Host -ForegroundColor Cyan "hello, world!"`; assert the Rust cell state
and the full PNG signature/size. Then run the endpoint over TCP and retain a
PNG artifact for the SFM consumer proof.

**Completion criteria:** A real Rust PTY session produces a bounded full PNG,
preserves terminal colors, and has no Java-owned terminal-content logic. The
isolated workspace does not introduce the incompatible `weavy`/Facet links
conflict into the primary workspace.

**Completion notes (2026-07-27):** The isolated
`tools/vox-terminal-server` workspace passed its focused puppet test and
`cargo fmt --check`. The test runs `1..100; Write-Host -ForegroundColor Cyan
"hello, world!"; exit`, verifies the bounded full-PNG payload, visible `100`,
and cyan cell metadata. The endpoint was also launched over loopback TCP and
consumed by the canonical SFM puppet; the Rust process remained hidden and the
SFM captures verified the live frame path. The endpoint source is committed
locally at `ff26237`; it is now also compiled into the primary CLI at the
aligned Facet revision. The isolated workspace remains only as a compatibility
test until the primary executable and SFM lifecycle proof are committed.

### [~] 4.4 Prove the optional frame consumer boundary

**Work:** Coordinate with the SFM Vox plan to consume the full PNG while Java
owns only input/resize plumbing, texture upload, clipping, GUI-scale/layout,
stale-frame rejection, and fallback. Rust never sends a Minecraft panel or
arbitrary UI instruction.

**Validation:** Run the existing SFM puppet/report flow for Java-local and
Rust-backed modes; include full-screen, nested, narrow, and supported GUI-scale
captures. Record the endpoint address and exact capture names.

**Completion criteria:** The Java-local mode remains useful with no Rust
process; the optional mode proves input round-trip, resize, disconnect/stale-
frame handling, and fallback without native GPU-handle interop. The completed
polling witness remains the baseline, but the accepted bridge uses the 3.6.2b
Vox channel subscription for live frames and the 3.6.2a retained-resource
rules; unary snapshots are explicit capture/resync operations only.

**Current evidence (2026-07-27):** Java accepts only bounded full PNG frames,
uploads them to a dynamic texture, explicitly binds the Minecraft shader
sampler, forwards text and logical resize requests, and preserves the
Java-local fallback. Focused SFM tests and two live Rust-backed captures pass.
Rust now accepts exact text, generic key transitions, and SGR/legacy mouse
events after terminal mode negotiation. SFM routes printable characters,
control/navigation key presses/releases, mouse buttons/drag/wheel events, and
periodically polls sequence-aware PNG snapshots. Commit `df80cb38d` fixed the
Java poller timing and made the automation proof wait for a command frame; the
live cyan capture now visibly contains Rust-rendered range and `Write-Host`
output. The deterministic focus-sequence tests cover triple Esc close and
triple Tab Java focus escape. A live Minecraft control-key/mouse/long-output
capture and stale/disconnect proof remain before this item is complete.

**Next integration correction — 2026-08-03:** The 50 ms poller is now a
historical evidence implementation, not the target. Complete 3.6.2a/3.6.2b,
import the reviewed Facet Java `Tx`/`Rx` runtime and generated subscription,
consume pushed frames on a dedicated Java receiver, and retain compatible
dynamic-texture resources. Remove periodic snapshot calls from the live path
while keeping the unary method for typed capture and full resynchronization.

### [~] 4.5 Consolidate the Rust endpoint into the normal CLI and lifecycle

**Work:** Promote the isolated endpoint into the primary `teamy-terminal`
binary as `teamy-terminal serve [ADDRESS]`, accepting `:PORT` as loopback
shorthand and defaulting to the configured development port. Align the
Facet/spec-proto/Vox revision with the primary workspace so the server no
longer requires a separate Cargo manifest. Retain the isolated workspace only
as a focused compatibility test until the primary command is proven.

Add CLI help/parse and loopback readiness tests. The SFM side will launch the
installed or configured `teamy-terminal.exe` directly, with no `cargo run` and
no extra Windows Terminal window. The server remains Rust-owned; Java only
chooses the endpoint, starts an optional child process, connects/reconnects,
and presents validated PNG frames.

**Validation:** Run `teamy-terminal serve :63946`, connect from SFM with Rust
started first, then start SFM first and invoke the Java start/connect action.
Stop and restart the endpoint while Minecraft remains open, and verify the
Rust terminal recovers without affecting `repl/open`.

**Current evidence (2026-07-27):** The primary `teamy-terminal.exe serve
[ADDRESS]` help and `:PORT` normalization are implemented, its 16 focused CLI
library tests pass, and the binary launches the endpoint under
`--stop-after-duration`. SFM's rebuilt `--solo --smoke` path reaches the title
screen with Vox Java present and Mekanism absent. The Rust-start puppet
`sfm:title_screen_rust_terminal` also completes with two live PNG captures,
including the `1..100` and cyan `Write-Host` proof, after Java launches the
primary executable. The remaining lifecycle gate is a focused stop/restart
matrix while Minecraft remains open; the correctness baseline is otherwise
proven before dirty-cell or GPU optimization.

**Completion criteria:** The primary executable exposes a bounded, documented
`serve` command; SFM's normal manual path uses a client-configured endpoint
and explicit `repl`/`terminal` actions; server startup after Java, server
failure/retry, and Java-only behavior are covered by focused tests and a live
capture. The full-PNG CPU path remains the correctness baseline before any
dirty-cell or GPU optimization.

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
| Windows x64 core/CLI | Supported first | workspace quality gate, PTY smoke, replay | Quality gate and PTY smoke pass; replay fixtures pass |
| Windows x64 interactive Vulkan MVP | Completed standalone baseline | PTY/Vulkan integration, command/resize/Ctrl+C/exit proof | Startup, renderer diagnostic, PTY smoke, live keyboard/paste/resize/alternate-screen/Ctrl+C/close, and app-owned selection/copy/zoom/visibility probes pass; native Ctrl+wheel is a manual UX smoke |
| Windows Vulkan device | Supported for windowed CPU-pixel presentation when loader/device exists; true GPU slug/off-screen comparison pending | renderer diagnostic, then Phase 3.6 GPU glyph and no-window/off-screen proof | Window startup smoke passes; true GPU glyph/off-screen path is Phase 3.6.4 |
| Windows without Vulkan/GPU | Supported for core/CLI; interactive renderer fails clearly | core/CLI pass; actionable renderer-construction error; no fallback | Invalid Vulkan ICD diagnostic passes; no fallback |
| Linux/macOS renderer | Not in first support slice | Do not claim support; later portability gate | Pending future phase |
| SFM Java-local terminal | Required independently of this repo | SFM Java tests and puppet | Tracked in Vox plan |
| SFM optional Rust/Vox mode | Optional development tooling; CPU full-PNG correctness, native-size, and correlated lag baselines proven | retained resources plus Vox push subscription, then Phase 3.6/V-4 renderer matrix | Input/lifecycle/full-PNG/native-size puppets pass; per-frame font reconstruction and 50 ms Java polling are the next repairs before comparative renderer evidence |

## Overall completion criteria

**Interactive MVP boundary:** The requested standalone-terminal goal is complete
at Phase 3.4 when `cargo run` opens a Vulkan window backed by the
default-profile PowerShell PTY and supports the agreed command/input/resize/
cancellation/exit behavior. Portable frame encoding, SFM integration, and
cross-platform renderer support are post-MVP work.

The roadmap as a whole is not complete: the standalone Phase 3.4 and 3.5.6
boundaries are complete, while Phase 3.6, the remaining Phase 4 bridge work, and Phase
5 remain open. The current performance work is audited against the
multi-second SFM lag witness and cross-renderer matrix, not retroactively folded
into the already-complete standalone MVP. The live keyboard/control boundary
and app-owned selection/copy/zoom/visibility boundary are green.

- [ ] All phase work items are [x] with local evidence.
- [ ] A fresh clone passes the workspace quality gate and exposes help/version.
- [ ] Core headless 1..1000 and process-free replay proofs are deterministic
  and bounded. The replay proof is complete; the overall criterion remains
  open until the broader phase evidence is green.
- [ ] Font extraction and Vulkan/off-screen output have deterministic fixtures.
- [ ] Portable frame data is validated without native GPU-handle interop.
- [x] The current SFM multi-second delay is reproduced with correlated
  Rust/Java stage evidence and no unsynchronized-clock arithmetic.
- [ ] Rust CPU/fontdue, Rust GPU/slug, Java vanilla-font, and Java Caskaydia
  candidates have matched semantic, visual, and temporal reports.
- [ ] The selected renderer/transport has no unexplained multi-second outlier
  in the matched workload and the rejected alternatives are documented.
- [ ] Stable-size frames reuse font/glyph/frame/texture resources, reversible
  resizes use bounded compatible caches, and live delivery uses bounded Vox
  `Tx`/`Rx` push with no periodic frame poller.
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
| Push publication queues one full PNG per PTY chunk | No debounce delay, but only one render/send in flight plus one newest pending sequence; use Vox credit and explicit coalescing counters before dirty/raw transports | Phase 3.6.2b–3.6.3 |
| Retained resources return stale glyphs, metrics, pixels, or leak across resize/backend changes | Complete cache keys include font/fallback/hash/size/backend; bound eviction; A→B→A tests; exact close/replacement counters | Phase 3.6.2a |
| Facet Rust supports channels while Java silently remains unary-only | Land credit-controlled Java `Tx`/`Rx` runtime/codegen tests and package the reviewed revision before generating/pinning the terminal subscription | Phase 3.6.2b/4.4 |
| A plausible `fontdue` hypothesis hides Java decode/upload or scheduling cost | Require the correlated current-path lag witness before changing defaults; report every major Rust/Java stage | Phase 3.6.1 |
| GPU glyph work looks fast internally but readback/encoding erases the gain | Measure GPU execution, readback, encode, transfer, Java upload, and end-to-end presentation separately | Phase 3.6.4–3.6.6 |
| Cross-font pixel differences create false failures or hide real terminal errors | Pair semantic cell assertions with geometry/color/occupancy and perceptual reports; use exact pixels only where valid | Phase 3.6.5 |
| Rust and Java monotonic clocks are treated as synchronized | Correlate ids/sequences, transmit local stage durations, and let Java measure its own end-to-end interval | Phase 3.6.1 |
| Review mistakes a smoke test for runtime wiring | Pair unit/fixture checks with CLI, renderer, and SFM puppet evidence at each boundary | All phases |

The colour-picker bridge remains deferred. The five-crate workspace, PTY smoke,
core VT state, bounded replay, embedded font, Vulkan presenter, portable frame
encoding, interactive wiring, CPU full-PNG SFM correctness proof, correlated
lag witness, native-size/cell-metric proof, retained resources, and bounded Vox
push delivery are implemented. The next required work is 3.6.3's named CPU
reference paths, followed by the parallel renderer/transport candidates defined
by Phase 3.6; release/propagation decisions follow the matched integration
matrix.
