# Platform PowerShell executable default

Tested source: `e9a0a5ab670fd0770e27e0e63ef94a29a4650984` on local branch
`task/platform-powershell-default-20261024`, based on published report
`44aa0a052d24615f85d475e14874918a0090d795`. A subsequent documentation commit
records these results; it is not another runtime-tested source. No push occurred.

The production PTY default previously hard-coded `pwsh.exe` on Linux. Earlier
qualification accidentally masked this with a local compatibility symlink.
`PtyConfig::default`, and therefore `interactive` and `test`, now select `pwsh`
on non-Windows hosts and `pwsh.exe` on Windows. The interactive error context now
names PowerShell rather than claiming every platform launches `pwsh.exe`.

This remains PowerShell-specific: arguments and readiness bootstrap do not support
arbitrary shells. The existing library `shell` field accepts an executable name or
unquoted path; `CommandBuilder` passes that path as one argument. Basename matching
controls bootstrap selection. No new CLI option is introduced: production currently
uses the default and the standalone server only exposes `--listen`. The focused
portability correction does not require a new configuration surface or validation
policy. Missing executables fail at spawn without silently choosing another shell.

## Qualification

The existing PowerShell 7.5.4 binary, Rust 1.96.0 and Temurin 21.0.8+9 were reused.
No installs, dependency changes, lock changes, downloads or security changes were
made. Qualification removed the original PowerShell installation directory from
PATH and exposed only a `pwsh` symlink to its existing real executable from a
separate directory named `powershell path qualification`. That directory contains
no `pwsh.exe`; `shutil.which('pwsh.exe')` returned `None`. This also exercises
actual executable resolution through a directory containing spaces. The old
`pwsh.exe` symlink was not removed from its original directory, but was inaccessible
through the test PATH and was not used.

- Negative control: the previously qualified `63a03c0` pinned server failed the
  real Java/TCP probe during `connect` with `INTERNAL`, unable to spawn `pwsh.exe`
  because no PATH candidate existed. This is an expected reproduced failure.
- Nine PTY tests pass, including platform defaults, space-containing explicit
  executable arguments, supported/unsupported basenames, missing executable,
  and actual delayed PSReadLine initialization with the socket barrier.
- Linux aggregate: **149 passed, 0 failed, 27 filtered** using
  `cargo test --offline --locked --workspace --all-features --no-fail-fast --
  --skip gpu --skip vulkan --skip v3_ --skip raster_renderer_leases
  --skip first_device_loss`. This retains readiness, malformed marker, session
  isolation, running-command cancellation, paste, selection and stream coverage.
- Strict workspace/all-targets/all-features Clippy and stable format check pass.
- Frozen-pin standalone server build and real Java/TCP readiness probe pass:
  14 explicit startup rejections, then two successful sends executed twice,
  CPU stream cancellation and post-close rejection.
- Separate scratch overlay of Vox macro source
  `e469b86cdbab46502b913fb2af48dfacbd8e26f0` passes the full malformed-first
  request/schema, Unicode PTY and CPU stream probe. Its immediate-input probe
  also passes (43 explicit rejections). Production pins remain unchanged.
- Independent read-only review found no blocking finding in the focused diff.

Windows defaults have conditional assertions but Windows compilation/execution
was not performed. GPU, desktop and Minecraft checks were not run. Filtered tests
are exclusions, not passes. SFM's executor still needs its separately approved
PowerShell availability and caller-policy work; this task did not edit SFM.

Receipts, exact commands and artifact hashes are in
[the evidence file](../evidence/platform-powershell-default-2026-10-24.json).
