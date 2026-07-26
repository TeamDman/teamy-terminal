# Core fixtures

Deterministic terminal transcripts and snapshots live here as the renderer-free
core contract. The CLI replay test consumes the transcript fixtures without
launching a process and compares the resulting snapshot to an independent
expected FNV-1a hash.
