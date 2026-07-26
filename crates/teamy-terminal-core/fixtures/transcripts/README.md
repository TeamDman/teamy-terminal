# Transcript fixtures

Deterministic terminal input/output transcripts belong in this directory.
Process-launching smoke fixtures remain outside the core crate.

The v1 line format supports:

- `size <columns> <rows> <max-scrollback-rows>`
- `write <hex-encoded-pty-bytes>`
- `resize <columns> <rows>`
- `event process-exit <code> <success>` and `event cancelled`
- `expect` lines for dimensions, cursor state, scrollback, lifecycle markers,
  and the full-snapshot hash
