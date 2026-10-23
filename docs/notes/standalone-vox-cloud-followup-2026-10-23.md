# Bounded cloud qualification follow-up

Runtime/test source: `fbd0975c57aad49a1ed24b6425a163f57c8e6ac7` on local branch
`migration/standalone-vox-consumer-2026-10-23`. Tests ran against that unchanged
source tree. This report is a separate documentation-only commit, not another
runtime qualification. No dependencies, locks or GPU fixtures changed in this
follow-up; the branch has not been pushed.

## Baseline comparison and repairs

The comparison used exact handoff `8aede3a196d46354e253ecc0e9446fd4ff00fe74` in a
separate detached worktree. Its unmodified test build failed with 16 Windows
library errors on Linux. To execute the comparison, only the same Windows-target
scoping of the CLI test dependency and failure diagnostics were applied locally.
Its lock and runtime source were retained. This was a baseline-plus-test-harness
comparison, not a claim that the unmodified baseline passed.

Both graphs failed the puppet's cyan assertion when inheriting `NO_COLOR=1`.
Captured shell bytes contained `hello, world!` without a color escape, while the
range output and frame arrived. With `NO_COLOR` unset for the test command, three
baseline runs passed. Three migrated runs instead reached the existing assertion
that at least one mutation must have coalesced. A fresh shell data/config/cache
run and a full aggregate migrated run passed that assertion. Shell history and
PTY chunking affect publication timing; a nonzero coalescing count cannot be
assumed from the real shell's particular output schedule.

The fixture now synchronously supplies two PTY output events after subscription
setup, before yielding its current-thread executor. This establishes a known
coalescing burst. It retains the nonzero coalescing assertion and all real shell,
color, range-output, frame, cancellation, content and RPC assertions. No timeout
was increased and no assertion was removed. Color remains an explicit test
environment prerequisite; product behavior still respects inherited `NO_COLOR`.

The initial frame-receive timeout did not recur in the diagnostic comparison or
final runs. Its original cause remains unproven; these results must not be
reported as establishing a timeout root cause. Timeout and color failures now
include raw PTY output and terminal state so a recurrence is actionable.

The Windows process ID field, local binding and initializer are now Windows-only,
matching their sole use in Windows process-tree termination. Unix process cleanup
is unchanged. The newly public server entrypoint also documents its error cases,
as required by strict Clippy.

## Final results

Rust 1.96.0 and the previously verified Linux PowerShell 7.5.4 were used. See the
[receipt](../evidence/standalone-vox-cloud-followup-2026-10-23.json) for log hashes.

- Puppet test: three consecutive passes with all assertions retained.
- Focused server/RPC/stream suite: 31 passed. The unsupported GPU lifecycle test
  was explicitly filtered in this focused run, then retained in the full run.
- Full workspace/all-features/no-fail-fast suite: **159 passed, one failed**.
  The remaining failure is
  `v3_switch_cpu_gpu_cpu_releases_idle_receiver_without_terminal_mutation`:
  advertised capabilities lack the required supported no-window GPU renderer.
  Its assertion remains active for supported hosts. This is not a green full gate.
- Strict workspace/all-targets/all-features Clippy: passed.
- Workspace/all-features build and standalone prototype check: passed.
- Stable formatting check: passed. Nightly-only import settings remain unverified.
- Windows-only tests are cfg-excluded on this host; passing conditional renderer
  tests does not establish GPU execution.

Reproduce after activating the Rust toolchain and making the verified PowerShell
available as `pwsh.exe`. Point XDG cache/config/data directories to writable test
locations. Unset `NO_COLOR` for these commands; do not alter product defaults:

```sh
env -u NO_COLOR cargo test --offline --locked --workspace --all-features --no-fail-fast
env -u NO_COLOR cargo test --offline --locked -p teamy-terminal-cli --all-features vox_server::tests -- --skip v3_switch_cpu_gpu_cpu_releases_idle_receiver_without_terminal_mutation
cargo clippy --offline --locked --workspace --all-targets --all-features -- -D warnings
cargo build --offline --locked --workspace --all-features
cargo fmt --all -- --check
```

The next qualification milestone needs supported GPU hardware/runtime and Windows
coverage. If the historical PTY timeout recurs, collect the new diagnostics and
compare the same shell state on the handoff baseline. Terminal-Java/SFM game
integration remains separate, unrun work. Consumer publication still needs
approval; no default merge or package publication is included.
