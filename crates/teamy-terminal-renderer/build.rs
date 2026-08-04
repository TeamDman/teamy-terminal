use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

const SHADER_PATH: &str = "shaders/gpu_slug.wgsl";
const ENTRY_POINT: &str = "gpu_slug_main";
const CELL_WORDS: usize = 12;
const MAX_COMBINING_GLYPHS: usize = 4;

fn main() {
    println!("cargo:rerun-if-changed={SHADER_PATH}");
    let source = fs::read_to_string(SHADER_PATH).expect("read rust-gpu-slug WGSL source");
    assert!(
        source.contains(&format!("const CELL_WORDS: u32 = {CELL_WORDS}u;")),
        "WGSL cell-word ABI changed without updating build.rs"
    );
    assert!(
        source.contains(&format!(
            "const MAX_COMBINING_GLYPHS: u32 = {MAX_COMBINING_GLYPHS}u;"
        )),
        "WGSL combining-glyph ABI changed without updating build.rs"
    );
    let module = naga::front::wgsl::parse_str(&source)
        .unwrap_or_else(|error| panic!("parse rust-gpu-slug WGSL: {error}"));
    let entry = module
        .entry_points
        .iter()
        .find(|entry| entry.name == ENTRY_POINT && entry.stage == naga::ShaderStage::Compute)
        .unwrap_or_else(|| panic!("WGSL must expose compute entry point {ENTRY_POINT}"));
    assert_eq!(
        entry.workgroup_size,
        [8, 8, 1],
        "shader workgroup ABI changed"
    );

    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    );
    let info = validator
        .validate(&module)
        .unwrap_or_else(|error| panic!("validate rust-gpu-slug WGSL: {error}"));
    let options = naga::back::spv::Options {
        lang_version: (1, 0),
        flags: naga::back::spv::WriterFlags::empty(),
        ..naga::back::spv::Options::default()
    };
    let pipeline = naga::back::spv::PipelineOptions {
        shader_stage: naga::ShaderStage::Compute,
        entry_point: ENTRY_POINT.to_owned(),
    };
    let words = naga::back::spv::write_vec(&module, &info, &options, Some(&pipeline))
        .unwrap_or_else(|error| panic!("emit rust-gpu-slug SPIR-V: {error}"));
    assert_eq!(
        words.first().copied(),
        Some(0x0723_0203),
        "invalid SPIR-V magic"
    );

    let source_hash = fnv1a64(source.as_bytes());
    let mut generated = String::new();
    writeln!(
        generated,
        "pub(crate) const GPU_SLUG_SHADER_ENTRY: &str = {ENTRY_POINT:?};"
    )
    .expect("write generated shader entry");
    writeln!(
        generated,
        "pub(crate) const GPU_SLUG_SHADER_CELL_WORDS: usize = {CELL_WORDS};"
    )
    .expect("write generated cell-word ABI");
    writeln!(
        generated,
        "pub(crate) const GPU_SLUG_SHADER_MAX_COMBINING_GLYPHS: usize = {MAX_COMBINING_GLYPHS};"
    )
    .expect("write generated combining-glyph ABI");
    writeln!(
        generated,
        "pub(crate) const GPU_SLUG_SHADER_SOURCE_FNV1A64: u64 = {};",
        readable_hex_u64(source_hash)
    )
    .expect("write generated source identity");
    writeln!(
        generated,
        "pub(crate) const GPU_SLUG_SHADER_WORDS: &[u32] = &["
    )
    .expect("write generated shader words header");
    for chunk in words.chunks(8) {
        generated.push_str("    ");
        for word in chunk {
            write!(generated, "{}, ", readable_hex_u32(*word))
                .expect("write generated shader word");
        }
        generated.push('\n');
    }
    generated.push_str("];\n");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("gpu_slug_shader.rs");
    fs::write(output, generated).expect("write generated rust-gpu-slug SPIR-V module");
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn readable_hex_u64(value: u64) -> String {
    format!(
        "0x{:04x}_{:04x}_{:04x}_{:04x}",
        value >> 48,
        (value >> 32) & 0xffff,
        (value >> 16) & 0xffff,
        value & 0xffff
    )
}

fn readable_hex_u32(value: u32) -> String {
    format!("0x{:04x}_{:04x}", value >> 16, value & 0xffff)
}
