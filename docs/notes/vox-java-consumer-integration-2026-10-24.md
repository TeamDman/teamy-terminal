# Java consumer integration and malformed-first response regression

The early-input behavior recorded here is addressed in the subsequent
[production input-readiness checkpoint](terminal-input-readiness-2026-10-24.md).
The malformed-first schema result remains separately qualified as described below.

The Terminal probe/test source is local commit
`5039d481a9a281e7a0b86429ddd5f41272d9a9d8` on
`task/vox-terminal-java-integration-20261024`. The Vox fix is local commit
`e469b86cdbab46502b913fb2af48dfacbd8e26f0` on
`task/malformed-first-response-schema-20261024` in TeamDman/vox.
Documentation follow-ups are separate from these tested source commits.
Neither branch has been pushed. Existing consumer manifests and locks are unchanged.

## Published input reconciliation

- Vox Java/runtime qualification: source `5628c00d9f3816199f324a9ccc081de8a3a1b1bc`,
  report `be6813e7a08c06376a8416eb3368bc97c6de1651`.
- Terminal published migration: `9078e66b1f327d919d19c4170dd038b9dbb84523`.
- SFM published migration: `0918604ac558906c93a9c4ab449400e8f75792ce`, inspected
  in a separate clean checkout. Its generated terminal client comes from the same
  qualified Java JAR and its typed CPU renderer identifier is `rust-cpu-fontdue`.
  Legacy frame events use backend identifier `rust.cpu.fontdue`; these identifiers
  serve different fields and must not be interchanged.
- Both consumers use registry Facet/core rc.7 and Taxon rc.5 with standalone
  Vox/Phon at the qualified source, plus the previously reviewed CSV source bridge.
  No additional dependency upgrade was made.

The SFM native-root TLS/real Minecraft echo follow-up is **not in this checkout**.
The coordinator reports successful 1.19.2 Forge 43.4.0 echo at implementation
`2e8e6141dd2a339289ef91a1781c2a63cecfcfac`, report
`8632d22e89476eaad87f80261e2916e690da8674`, branch
`task/parchment-handshake-20261023`. Object lookup could not resolve its
implementation from the public repository. Do not treat the published SFM
checkpoint as containing that fix, or infer that its historical handshake blocker
is still the latest overall result. Source reconciliation requires the other
executor's commits to be provided or published with approval; no game rerun is
needed merely to repeat the coordinator's existing handshake evidence.

## Reproduced failure and local fix

The new Java probe sends empty argument bytes as the first `get_content` call,
receives `InvalidPayload`, then makes a valid call for a missing session. On the
published Rust dispatcher, that valid call fails in Java with
`method payload schema is incompatible`, caused by `trailing bytes: 84`.

Generated Rust argument-decode rejection advertised
`Result<ActualSuccess, VoxError<Infallible>>`, omitting the declared application
error type. That first response established the lane's method response binding.
A later application-error response could no longer be decoded under that binding.
The local macro fix preserves the actual error type for both borrowed and retained
argument decode paths. It follows `schema.tracking.bindings` and `rpc.error.scope`.
Normal method signatures and Java artifact bytes do not change. Consumers must
rebuild their generated Rust dispatchers with the fixed macro; changing only the
Java runtime does not repair this defect.

The qualification build used published Terminal source `9078e66...` in an isolated
checkout with a Cargo command-line path patch for `teamy-vox-macros-core` to local
Vox source `e469b86c...`. The unchanged local macro parser dependency accompanies
that path override. Only that scratch checkout's lock changed for this overlay.
This is **not** a successful test of the unchanged published consumer dependency.
The new probe intentionally still fails against that published dependency.

## Shell startup finding

Before waiting for shell readiness, two real Java/TCP runs timed out after the
command was echoed before PowerShell's prompt, with no sentinel output. A first
frame and `connect` success establish a session, not completion of shell startup.
The probe now waits for the actual default PowerShell prompt before sending input;
its `--early-input` diagnostic mode retains the earlier sequence. The Rust puppet
fixture now observes the prompt too. Neither frame timeout was increased, and
all existing assertions remain. The sentinel is assembled inside PowerShell so
command echo cannot satisfy the output witness.

This addresses test synchronization; production handling of input sent during
shell startup is still an unresolved behavior. These observations are consistent
with the earlier timeout, but do not prove every historical timeout had this cause.
A final `--early-input` run with the fixed macro reproduced the same timeout and
echo-before-prompt content, independently of the repaired schema bug. Custom
shell prompt behavior is outside this default-PowerShell probe.

## Results and limits

- Published-source A/B probe: **failed**, reproducibly, on the second call after
  malformed-first input. Its initial `InvalidPayload` is not an overall pass.
- Local macro overlay: **passed** twice with the complete regression probe;
  final run used the committed probe source above. Checks include generated schema
  negotiation, unsupported-renderer application error, missing/foreign-session
  rejection, Unicode PTY output, bounded pushed CPU PNG, receiver-close completion,
  continued connection use and post-disconnect session rejection.
- Vox macro snapshots: 25 passed, including retained fallible decode; core: 88 passed.
- Vox generated-service integration: 10 passed; schema compatibility: eight passed.
- Terminal synchronized puppet: passed; local control RPC: eight passed;
  strict workspace/all-targets/all-features Clippy: passed; stable format: passed.
- Java probe compiles with Temurin 21.0.8+9, `--release 17 -Xlint:all -Werror`.
  Qualified JAR SHA256 remains
  `be441a57e11d4981d283306fe471874e625bf2f423c83a91ce8f44dde623d062`.
- No GPU, Minecraft, desktop, package installation, publication or merge ran.
  Full aggregate tests were not repeated because the existing suite includes GPU
  work. Windows and nightly formatting remain unrun. Tracey skill/MCP were not
  available; spec references were checked manually, not machine-verified.

The [receipt](../evidence/vox-java-consumer-integration-2026-10-24.json) records
binary and log digests. The probe is a real Terminal TCP client, not an execution
of SFM's Java UI adapter or Minecraft. The existing SFM control/Minecraft evidence
remains separately attributed.

## Reproduction and next integration step

Build the prototype from its pinned lock for the failing baseline. For the fixed
comparison, use a separate checkout and this temporary build override:

```sh
cargo build --offline --manifest-path tools/vox-terminal-server/Cargo.toml \
  --config 'patch."https://github.com/TeamDman/vox.git".teamy-vox-macros-core.path="/path/to/vox/rust/vox-macros-core"'
python3 scripts/vox-java-terminal-probe/run.py \
  --java-home /path/to/temurin-21.0.8+9 \
  --runtime-jar /path/to/teamy-vox-java-0.10.0-rc.7.jar \
  --server /path/to/rebuilt/teamy-terminal-vox-server \
  --output /path/to/new-evidence-directory
```

Use Rust 1.96.0 and make the existing verified Linux PowerShell available as
`pwsh.exe` on PATH. The runner uses fresh writable XDG directories, owns only its
loopback server process, bounds execution, and records passed or failed receipts.

After review and separate publication approval, publish the Vox fix, advance both
consumer Rust pins/locks coherently, rebuild and rerun this probe without overrides.
Integrate the other executor's SFM follow-up first or alongside that change without
clobbering it. Keep the Java artifact pin unchanged unless its source/artifact
recipe is intentionally revised and requalified. No main merge is implied.
