# Rust GPU Slug provenance and coverage contract

This note freezes the source and data-contract provenance for Teamy Terminal
plan item 3.6.4a. It does not claim that the Vulkan renderer in 3.6.4c exists.

## Read-only algorithm reference

- Repository: `https://github.com/TeamDman/Teamy-Studio.git`
- Revision inspected: `c827d78c4b60b64348a82db9c7a961c01c8d0701`
- Commit date: 2026-07-26 21:53:56 -0400
- Code license: MPL-2.0 (`LICENSE` and the workspace `license` field)
- Primary source: `crates/teamy_studio_shell/src/windows_d3d12_renderer.rs`
- Design references: `docs/notes/slug-font-renderer.md` and
  `docs/notes/text-rendering-glyphy-band-contract-plan.md`
- Functions and types inspected: `QuadraticCurve`, `QuadraticCurveBuilder`,
  `build_slug_curve_buffer`, `extract_glyph_curves`,
  `append_slug_band_data`, `choose_horizontal_band_split`,
  `choose_vertical_band_split`, `choose_band_split`,
  `load_directional_band_header`, `cpu_slug_coverage`,
  `cpu_slug_coverage_all_curves`, and their curve-coverage helpers.

The Teamy Studio worktree was at the revision above but was not globally clean:
`Cargo.toml` had an unrelated four-line diff (two insertions and two deletions).
The renderer source and both design-note paths listed above had no worktree diff.
All inspected reference content was therefore read from the exact commit with
`git show`; no Teamy Studio file was edited.

Teamy Studio loaded an installed `CaskaydiaCove Nerd Font Mono` through
`fontdb`, so its worktree did not provide immutable font bytes. Teamy Terminal
instead uses its existing embedded deterministic face:

- File: `crates/teamy-terminal-font/assets/CaskaydiaCoveNerdFontMono-Regular.ttf`
- Upstream archive: Nerd Fonts v3.4.0 `CascadiaCode.zip`
- SHA-256: `32aa528c1d9be2240ceac90aa05f4e554679cabeb11b93684eb24ec4930bd0ea`
- Font license: SIL Open Font License 1.1, preserved in
  `CaskaydiaCoveNerdFont-LICENSE.txt`

## Owned renderer-neutral contract

`teamy_terminal_font::slug` owns the reusable boundary. It parses outlines with
`ttf-parser`, represents lines as degenerate quadratics, recursively
approximates cubic segments, records explicit fallback resolution, and builds
horizontal and vertical directional bands. Every band header has exactly four
fields: curve count, descending-list offset, ascending-list offset, and split.
Maximum-descending and minimum-ascending lists are distinct, and split search
minimizes the worse candidate ray count.

The all-curves CPU oracle never accepts caller-supplied or serialized band
data. It derives ray splits from validated source curves and then walks every
curve; packed offsets, curve-index lists, early exits, and malformed GPU words
cannot alter its result. The banded evaluator separately validates and consumes
the directional representation, allowing parity tests to detect pruning,
ordering, or serialization defects.

The contract is explicitly GPU-serializable without depending on `ash`:

- each quadratic exports as two `vec4<f32>` records;
- band tables export as canonical `u32` words with absolute packed offsets;
- fixed 16-word glyph metadata carries curve/band starts and counts, fallback
  flags, units per em, advance, bounds, and band transforms; and
- fixed font words carry units per em, ascender, descender, and vertical span.

These values are sufficient for a later compute shader to map cell-local pixel
coordinates into font units. Vulkan device, shader, buffer, and lifecycle
ownership remains outside the font crate and outside this change.

## Regression evidence

`tests/fixtures/slug-g-false-line-regression-v1.json` is a checked,
machine-readable 96-by-96 sample-grid witness for the historical internal `g`
false-line defect. It records the embedded font identity, directional-list
diversity, all-curves parity, false-positive pixel/run counts, and a stable
quantized coverage hash. Focused tests also cover `/`, `b`, `r`, `6`, ASCII,
box and Powerline characters, Unicode/combining/fallback behavior, empty and
flat glyphs, cubic approximation, malformed geometry, invalid bounds, corrupt
headers/offsets/indices, and the GPU word layout.
