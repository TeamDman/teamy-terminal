# Production admission contract for input during shell startup

Tested source checkpoint: `95acc6faff29f6d9768f929497b1f3b9f6fab1e9`, on local
branch `task/terminal-input-readiness-20261024`. This documentation follow-up is
not another runtime-tested source commit. The prior Vox macro fix and its isolated
qualification checkout remain preserved. Nothing was pushed or merged.

## Contract

A successful `connect` creates a session. Its existing `Ready` lifecycle value and
first frame do **not** promise shell input readiness. The server now enforces that
boundary rather than relying on callers or tests to wait for visible prompt text.

Each session starts with its own closed input gate. The existing PowerShell
bootstrap emits OSC 133 prompt markers; the terminal's streaming parser must see
one complete A/B prompt before the gate opens. Ordinary prompt-looking text and
partial escape sequences do not admit input. The check and subsequent operation
hold the same session mutex. Internal terminal-query replies remain permitted,
so shell startup itself can progress.

| State | Text/key/mouse/paste/Ctrl+C request | Retry policy |
| --- | --- | --- |
| Starting, within 30 seconds | `InvalidRequest`, `retryable=true`; no bytes forwarded, queued or replayed | Caller may retry the rejected request after a readiness notification/frame or with bounded backoff |
| First complete prompt received before deadline | Existing forwarding behavior | Each accepted send is a distinct operation |
| No prompt within 30 seconds | `Disconnected`, `retryable=false`; no bytes forwarded | Disconnect and create a new session |
| PTY exited, errored or session closed | `Disconnected`, or `SessionNotFound` after removal | Do not replay into the closed session |

The deadline is monotonic and checked on admission/prompt observation; there is no
background input queue, unbounded pending future or startup timer task. Timeout is
latched, and a late marker cannot reopen that session. Read/render/resize and
explicit disconnect remain available during startup or for diagnosis. The timeout
does not automatically destroy the diagnostic session; the caller should disconnect.

Cancellation before readiness is also rejected: it must not inject Ctrl+C into a
half-initialized shell. Disconnect is the operation for aborting startup. There are
no deferred input operations to cancel or accidentally flush later. Closing after
PTY failure now runs cleanup exactly once: a separate shutdown latch prevents the
preexisting `closed` flag from skipping process cleanup.

## Compatibility and client responsibility

No wire fields, method IDs, enum variants, generated bindings, dependency manifests
or lockfiles changed. This is an observable behavior correction for early callers:
a request that could previously report `Forwarded` while being lost now returns an
explicit application rejection. Existing Java clients can decode it using the
existing `TerminalError.code` and `retryable` fields. Callers must honor the
retryable rejection rather than treating every `InvalidRequest` as permanently
fatal. SFM UI retry/notification behavior was not changed or game-tested here.

An empty text request may be used as a non-mutating readiness probe. Only explicit
startup rejection establishes that nothing was forwarded. A successful send still
acknowledges forwarding, not command completion; a transport timeout or lost reply
must not trigger blind replay. Application `client_sequence`/correlation IDs are
not deduplication keys. Two successful calls with the same application IDs remain
two operations; generated wire request IDs remain distinct.

The first readiness marker is latched for the session, so running commands still
accept input and cancellation. Marker detection uses parser metadata without
copying the terminal cell buffer. Custom prompt text is supported because the gate
does not inspect visible strings. A profile/bootstrap that fails to emit the
required markers fails closed after the deadline. Windows/ConPTY execution remains
unqualified by this Linux run.

## Qualification

All tests use the existing Rust 1.96.0, Temurin 21.0.8+9 and Linux PowerShell 7.5.4.
No tools were installed. Java compiles with `--release 17 -Xlint:all -Werror` and
uses the previously qualified JAR unchanged.

- Four state-machine tests use supplied monotonic instants, not sleeps, to prove
  delayed readiness, the exact deadline, late markers, closure and isolation.
- Five service tests use a controlled PTY child and actor notifications. They
  cover every rejected input route, partial markers, custom prompt text, repeated
  rejected and accepted sends, no queued replay, actual child exit before prompt,
  simulated reader failure, startup timeout, repeated close and foreign-session
  rejection. No fixed readiness delay is used.
- Two core OSC marker tests pass.
- Five existing behavior tests pass: real PowerShell PNG puppet, running-command
  cancellation, guarded paste, authoritative selection/copy and mouse reporting.
- Strict workspace/all-targets/all-features Clippy, pinned prototype build and
  stable format check pass. Nightly-only formatting options remain unverified.
- The new Java/TCP readiness probe calls `send_text` immediately after session
  creation. It allows only one in-flight call, retries only explicit startup
  rejection, and uses no prompt matching or readiness sleep. A counter evaluated
  inside PowerShell proves that two accepted same-ID sends execute exactly twice;
  terminal repainting cannot invalidate this execution witness.
- Against the prior server with the existing Vox macro fix, the probe fails:
  input is acknowledged, echoed before the prompt, and never executed.
- Against the new server with **unchanged published dependency pins**, three fresh
  shell runs pass (32, 16 and 14 explicit startup rejections). CPU PNG delivery,
  receive-side cancellation and post-disconnect rejection pass in each run.

The separate complete malformed-first Java regression is also retained and tested
with the local Vox macro overlay at `e469b86cdbab46502b913fb2af48dfacbd8e26f0`.
That overlay is still needed for the distinct schema bug; it is not needed for
this readiness fix. See the [machine receipt](../evidence/terminal-input-readiness-2026-10-24.json)
for final overlay results, binary digests and exact log hashes. Tests ran against
the unchanged production source subsequently committed above; the final combined
overlay build used an isolated checkout of that exact commit.

No full aggregate/GPU run, Minecraft, desktop, Windows, installation, network
setting change, package publication or default merge was performed. The full
aggregate suite includes the previously known unsupported GPU test, so targeted
CPU and input checks were used. Passing these checks is not a claim of full
platform qualification.

## Reproduce

With the existing toolchain activated and verified PowerShell exposed as
`pwsh.exe` on PATH:

```sh
cargo test --offline --locked -p teamy-terminal-cli --lib shell_input
cargo test --offline --locked -p teamy-terminal-cli --lib startup_tests
cargo build --offline --locked --manifest-path tools/vox-terminal-server/Cargo.toml
python3 scripts/vox-java-terminal-probe/run.py \
  --java-home /path/to/temurin-21.0.8+9 \
  --runtime-jar /path/to/teamy-vox-java-0.10.0-rc.7.jar \
  --server /path/to/rebuilt/teamy-terminal-vox-server \
  --readiness --output /path/to/new-evidence-directory
```

Use the existing isolated macro path override described in
[the Java integration report](vox-java-consumer-integration-2026-10-24.md) when
running the separate malformed-first probe without `--readiness`. Consumer pins
remain frozen here. Publication and coordinated consumer repinning require
separate approval; SFM's unpublished TLS follow-up is independent of this fix.
