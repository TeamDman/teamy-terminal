# Reviewed shell-readiness checkpoint

Tested Terminal source: `63a03c0a3f43e541860b7a31597d44e4bbba2c69` on local
`task/terminal-input-readiness-20261024`. The later report commit is documentation,
not another runtime-tested source. No push, merge, install, dependency or lockfile
change occurred. Overall Terminal/SFM consumer integration is **not ready** until
the SFM caller work below is qualified. This supersedes the marker mechanism in
[the first readiness report](terminal-input-readiness-2026-10-24.md).

## Reviewed production contract

`connect` still means attached, not input-ready. Every external input route checks
one session-local gate under the same mutex as forwarding. Starting rejects with
existing `InvalidRequest`, `retryable=true`, and an explicit no-forward message.
There is no queue or automatic replay. The 30-second monotonic deadline and close
remain terminal: late signals cannot reopen a timed-out/closed session. A caller
may retry a specifically rejected operation; it must not replay an ambiguous
transport failure. Successful repeated application IDs still represent distinct
operations, not deduplication keys. No wire schemas changed.

Ordinary OSC133 A/B no longer opens the gate. Each process receives a fresh 32-hex
RandomState-based discriminator. The injected PowerShell read hook emits exact
`OSC 1337;TeamyShellReady;<token>` after the prompt and before delegating to the
original reader. The VTE parser compares all three fields and latches readiness
independently of screen metadata. Adjacent output is untouched, and the signal is
not visible in terminal cells. It remains present in the existing bounded raw
debug-output buffer. This discriminator prevents accidental, stale and other-session
signals; it is **not authentication against a hostile profile in the same process**.

The initial hook enables `Console.TreatControlCAsInput` before signaling, then
restores its saved value in `finally` when the first reader returns or throws.
Subsequent calls delegate unchanged. Restoration matters: keeping that setting
true breaks interruption of executing commands. The wrapper executes bootstrap
work before its first delegation, so a custom reader must not rely on the incoming
first-call PowerShell `$?` value. No such guarantee is added; subsequent calls retain
the original delegation behavior. Missing/nonfunctional hooks fail closed through
the existing deadline. Windows/ConPTY behavior has not been executed here.

VTE recognizes OSC termination on ESC before the following ST backslash. Tests
assert these parser semantics rather than inventing a second raw-byte parser.
Terminal query replies now drain the parser's response queue, so split cursor and
device queries work and cursor coordinates reflect the parsed terminal state.

## Validation and review

Existing Rust 1.96.0, Temurin 21.0.8+9, PowerShell 7.5.4 and unchanged Java runtime
JAR from Vox `5628c00d9f3816199f324a9ccc081de8a3a1b1bc` were used. Durable hashes,
commands and Java receipts are in [the evidence JSON](../evidence/terminal-readiness-review-2026-10-24.json).

- Linux workspace/all-features aggregate: **146 passed, 0 failed, 27 explicitly
  filtered**. Command: `cargo test --offline --locked --workspace --all-features
  --no-fail-fast -- --skip gpu --skip vulkan --skip v3_ --skip
  raster_renderer_leases --skip first_device_loss`. GPU/runtime lifecycle checks
  are excluded, not passing. Windows cfg tests and opt-in ignored platform
  integration binaries were not executed.
- Strict workspace/all-targets/all-features Clippy and stable formatting pass.
- Marker tests cover all split positions with BEL and ST, wrong session/token,
  extra fields, truncated and aborted OSC, adjacent visible text, scroll/reset,
  resize, and fragmented query replies. Existing actor tests cover rejection on
  every input route, startup failure, actual child exit, timeout, repeated close,
  cancellation before readiness, isolation and repeated successful sends.
- A real PowerShell test blocks a custom reader on a TCP socket barrier before
  invoking actual PSReadLine. Input is written before releasing that barrier and
  executes successfully. No readiness sleeps are used. The ordinary aggregate
  also exercises interruption of a running PowerShell command.
- Two real generated-Java/TCP readiness runs on frozen pins pass: 34 and 15
  explicit rejections, followed by exactly two executions of two accepted sends,
  CPU streaming, receiver cancellation and post-close rejection.
- Separate scratch overlay of Vox macro source
  `e469b86cdbab46502b913fb2af48dfacbd8e26f0` passes full malformed-first-request
  schema regression and readiness probe (14 explicit rejections). The overlay
  changes only scratch resolution; no production pin or lock was updated.
- Independent read-only review inspected the original gate, then the revised
  parser/hook and final restoration commit. It found no remaining blocking defect
  in the final Terminal diff. Review is not a substitute for platform execution.

Failure history is retained rather than hidden: the original ordinary-marker gate
admitted profile output and depended on chunk-end prompt metadata. The aggregate
also exposed a selection fixture selecting the live prompt instead of its synthetic
text; that fixture now clears and positions its own text explicitly. An intermediate
control-mode change at `81c3e59` broke running-command cancellation; source
`63a03c0` restores the mode and passes the affected test and aggregate. An initial
ST test incorrectly assumed VTE waited for the backslash; its assertion now
matches VTE semantics. No timeouts were increased to obtain a pass.

## Remaining consumer work and continuation

Published SFM `0918604ac558906c93a9c4ab449400e8f75792ce` is available locally, but
has not been edited in this checkpoint. In the authored template
`platform/minecraft/core-liquid-template/src/main/java/ca/teamdman/sfm/client/terminal/SFMVoxTerminalService.java`:

1. `requireSuccess` drops `TerminalError` fields by throwing plain
   `IllegalStateException`; preserve typed code/retryable information. The 1.19
   branch already has `TerminalApplicationException` for resize failures.
2. `enqueueRemoteOperation`/`failTransportIfCurrent` and later-version synchronous
   catches close the transport on startup rejection. Keep the same session and
   streams for explicit no-forward rejection, show a failure, and permit a later
   caller retry. Do not replay generic errors/timeouts. Preserve generation checks.
3. Async `sendText(true)` means locally enqueued, not remote acceptance. An empty
   string short-circuits locally, so it cannot probe readiness through this adapter.
4. Paste callbacks currently run only on success; the public result has only
   `PASTED`/`CONFIRMATION_REQUIRED`. Define an explicit rejection completion and
   update its callers so startup rejection cannot leave a pending interaction.
5. Qualify rejection -> same-session success, transient error clearing, no replay,
   timeout -> ordinary disconnect, and paste completion with the CPU adapter tests
   and native projection workflow across affected authored version branches. This
   is pending implementation/qualification, not an approval denial or a claimed pass.

The separate SFM TLS implementation `2e8e6141dd2a339289ef91a1781c2a63cecfcfac` and
report `8632d22e89476eaad87f80261e2916e690da8674` remain unpublished in task
`01a16948-6a45-7223-b355-dd25dab010e3`; they are not in this checkout. Parent-reported
Minecraft handshake evidence is not rerun here. Obtain those committed changes
through coordination before reconciling SFM runtime integration; they do not block
the independent caller-policy fix. No desktop, GPU or game execution was attempted.
