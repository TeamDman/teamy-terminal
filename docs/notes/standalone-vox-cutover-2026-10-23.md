# Standalone Vox consumer qualification

Source baseline: `8aede3a196d46354e253ecc0e9446fd4ff00fe74`, published on
`migration/vox-consumer-source-handoff-2026-10-23`. The local migration source
commit is `b315215f228513f2fda21971e9fc544b631d45ce`; validation ran against its
unchanged source tree. This documentation follow-up is not another runtime test.
The changed consumer branch is local only; publication requires approval.

Root streaming, control and the standalone prototype now use TeamDman/vox
`5628c00d9f3816199f324a9ccc081de8a3a1b1bc`. The prototype calls the CLI's public
server entrypoint instead of including its private source file without its
surrounding modules. Windows test dependencies are Windows-target scoped.
No protocol compatibility adapter or test fixture changes were introduced.

Both resolved graphs have one registry `teamy-facet`/core rc.7 identity, one
registry `taxon` rc.5 identity and one standalone Vox/Phon source revision.
Taxon is unified, not a deliberate old/new boundary. Cancellation retains its
existing source pin; this baseline owns StopAfterArgs locally.

CSV needs a source exception: registry CSV rc.8 pins format rc.8 while JSON/Figue
require format rc.9. CSV at public facet-format commit
`642d268fc01eced0a861bded044342ff88733eec` aligns with rc.9. Workspace-root patches
resolve that source's sibling dependencies to the same registry Facet/core,
reflect and format identities. The separate prototype repeats those patches.

## Observed validation

See the [receipt](../evidence/standalone-vox-cutover-2026-10-23.json) for exact
identities, source revisions and original log hashes. Rust 1.96.0 was used.

- `cargo build --offline --locked --workspace --all-features`: passed.
- `cargo check --offline --locked --manifest-path tools/vox-terminal-server/Cargo.toml`: passed.
- Standalone prototype build and `--help`: passed.
- Control suite: eight tests passed, including actual local transport RPC.
- `cargo test --offline --locked --workspace --all-features --no-fail-fast`:
  158 passed, two failed. This is **not** a green consumer qualification.
- `v3_switch_cpu_gpu_cpu_releases_idle_receiver_without_terminal_mutation`
  requires a supported no-window GPU renderer unavailable in this environment.
- `vox_puppet_runs_power_shell_and_returns_bounded_png` timed out awaiting a PTY
  mutation frame; it also failed alone. Cause remains unresolved. Do not assume
  this is a protocol regression or an environment-only issue without evidence.
- Strict workspace/all-targets/all-features Clippy fails on the existing
  Unix-unused `PtySessionControl.process_id` field. That source is unchanged.
- Stable `cargo fmt --all -- --check` passed; nightly-only import options were
  ignored. The repository's nightly formatting check remains unrun.

PowerShell fixtures used the official Linux PowerShell 7.5.4 distribution with
verified archive SHA256, a local `pwsh.exe` symlink and writable XDG cache/config/
data directories. Initial missing-shell failures are superseded by the real-shell
run above. Windows-specific tests were cfg-excluded. Conditional renderer tests
passing on unavailable hardware do not establish GPU execution.

## Continuation

Retain the failed fixtures. Diagnose the isolated PTY frame timeout with shell
readiness/output evidence and compare baseline behavior before attributing it to
Vox. Qualify Windows behavior, actual GPU lifecycle and nightly formatting in an
appropriate environment, then rerun broader checks after any repairs. The library
has separately qualified 12 Rust-Java TCP RPC tests; terminal/SFM Java end-to-end
integration and game execution have not been established by this consumer run.

SFM preparation is a separate unapplied proposal against public capture SHA
`8e349578d18511d8d5993b66a3ad3c7c957e5db4`. Coordinate with the completed capture
checkpoint before changing its CLI locks, authored Java generation or selected
Java toolchain recipe. Use the tested Vox source above, preserve frozen release
recipes and Liquid templates, and perform game validation in the SFM environment.
Do not publish consumer branches or packages without confirmed approval.
