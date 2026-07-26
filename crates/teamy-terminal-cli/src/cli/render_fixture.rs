use crate::cli::output::CliOutput;
use arbitrary::Arbitrary;
use eyre::Context;
use facet::Facet;
use figue::{self as args};
use std::fs;
use std::path::PathBuf;
use teamy_terminal_core::TerminalLimits;
use teamy_terminal_core::TerminalSession;
use teamy_terminal_font::DEFAULT_PIXEL_SIZE;
use teamy_terminal_font::TerminalFont;
use teamy_terminal_frame::FrameCompression;
use teamy_terminal_renderer::OffscreenFrame;

/// Output format for the deterministic render fixture.
#[derive(Arbitrary, Clone, Copy, Debug, Default, Facet, PartialEq, Eq)]
#[facet(rename_all = "kebab-case")]
#[repr(u8)]
pub enum RenderFixtureFormat {
    /// Lossless PNG archive/keyframe output.
    #[default]
    Png,
    /// Tight row-major RGBA8 bytes.
    Raw,
}

/// Arguments for `teamy-terminal render-fixture`.
#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
#[facet(rename_all = "kebab-case")]
pub struct RenderFixtureArgs {
    /// Artifact encoding.
    #[facet(args::named, default)]
    #[arbitrary(default)]
    pub format: RenderFixtureFormat,

    /// Output path; defaults to a format-specific file in the current dir.
    #[facet(args::named)]
    #[arbitrary(default)]
    pub output: Option<String>,
}

#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
struct RenderFixtureOutput {
    path: String,
    format: RenderFixtureFormat,
    sequence: u64,
    width: u32,
    height: u32,
    stride: u32,
    bytes: usize,
}

impl RenderFixtureArgs {
    /// Render and write the deterministic fixture.
    pub fn invoke(self) -> eyre::Result<CliOutput> {
        let mut terminal = TerminalSession::new(TerminalLimits::default(), 40, 12);
        terminal.process_bytes(
            b"\x1b[1;34mTeamy Terminal\x1b[0m\r\nportable frame fixture\r\n$ echo ready",
        );
        let snapshot = terminal.snapshot();
        let mut font = TerminalFont::new(DEFAULT_PIXEL_SIZE)?;
        let width = u32::from(snapshot.columns) * font.cell_width();
        let height = u32::from(snapshot.rows) * font.cell_height();
        let frame = font.render(&snapshot, width, height);
        let compression = match self.format {
            RenderFixtureFormat::Png => FrameCompression::Png,
            RenderFixtureFormat::Raw => FrameCompression::Raw,
        };
        let packet = OffscreenFrame::from_rasterized(frame)?.full_packet(1, compression)?;
        let path = self
            .output
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(self.format.default_path()));
        fs::write(&path, packet.bytes())
            .wrap_err_with(|| format!("failed to write render fixture at {}", path.display()))?;
        Ok(CliOutput::facet(RenderFixtureOutput {
            path: path.display().to_string(),
            format: self.format,
            sequence: packet.descriptor.sequence,
            width: packet.descriptor.width,
            height: packet.descriptor.height,
            stride: packet.descriptor.stride,
            bytes: packet.bytes().len(),
        }))
    }
}

impl RenderFixtureFormat {
    const fn default_path(self) -> &'static str {
        match self {
            Self::Png => "teamy-terminal-fixture.png",
            Self::Raw => "teamy-terminal-fixture.rgba",
        }
    }
}
