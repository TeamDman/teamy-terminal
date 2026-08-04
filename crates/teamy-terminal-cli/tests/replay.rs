use eyre::Result;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use teamy_terminal_core::Color;
use teamy_terminal_core::TerminalLimits;
use teamy_terminal_core::TerminalSession;
use teamy_terminal_core::TerminalSnapshot;

#[derive(Debug)]
struct Fixture {
    columns: u16,
    rows: u16,
    max_scrollback_rows: usize,
    operations: Vec<Operation>,
    expected: Expected,
}

#[derive(Debug)]
enum Operation {
    Write(Vec<u8>),
    Resize { columns: u16, rows: u16 },
    ProcessExit { code: u32, success: bool },
    Cancelled,
}

#[derive(Debug, Default)]
struct Expected {
    columns: Option<u16>,
    rows: Option<u16>,
    cursor_column: Option<u16>,
    cursor_row: Option<u16>,
    cursor_visible: Option<bool>,
    scrollback_rows: Option<usize>,
    hash: Option<u64>,
    process_exit: Option<(u32, bool)>,
    cancelled: Option<bool>,
}

fn parse_fixture(path: &Path, contents: &str) -> Result<Fixture> {
    let mut size = None;
    let mut operations = Vec::new();
    let mut expected = Expected::default();

    for (line_number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = line.split_whitespace().collect::<Vec<_>>();
        match fields.as_slice() {
            ["size", columns, rows, max_scrollback_rows] => {
                size = Some((
                    columns.parse::<u16>()?,
                    rows.parse::<u16>()?,
                    max_scrollback_rows.parse::<usize>()?,
                ));
            }
            ["write", bytes] => operations.push(Operation::Write(decode_hex(bytes)?)),
            ["resize", columns, rows] => operations.push(Operation::Resize {
                columns: columns.parse()?,
                rows: rows.parse()?,
            }),
            ["event", "process-exit", code, success] => {
                operations.push(Operation::ProcessExit {
                    code: code.parse()?,
                    success: success.parse()?,
                });
            }
            ["event", "cancelled"] => operations.push(Operation::Cancelled),
            ["expect", "columns", columns] => expected.columns = Some(columns.parse()?),
            ["expect", "rows", rows] => expected.rows = Some(rows.parse()?),
            ["expect", "cursor", column, row] => {
                expected.cursor_column = Some(column.parse()?);
                expected.cursor_row = Some(row.parse()?);
            }
            ["expect", "cursor-visible", visible] => {
                expected.cursor_visible = Some(visible.parse()?);
            }
            ["expect", "scrollback", rows] => expected.scrollback_rows = Some(rows.parse()?),
            ["expect", "hash", hash] => {
                expected.hash = Some(u64::from_str_radix(hash, 16)?);
            }
            ["expect", "process-exit", code, success] => {
                expected.process_exit = Some((code.parse()?, success.parse()?));
            }
            ["expect", "cancelled", cancelled] => {
                expected.cancelled = Some(cancelled.parse()?);
            }
            _ => eyre::bail!(
                "{}:{}: unsupported replay fixture line: {line}",
                path.display(),
                line_number + 1
            ),
        }
    }

    let Some((columns, rows, max_scrollback_rows)) = size else {
        eyre::bail!("{}: missing size", path.display())
    };
    Ok(Fixture {
        columns,
        rows,
        max_scrollback_rows,
        operations,
        expected,
    })
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    if value.len() % 2 != 0 {
        eyre::bail!("hex fixture value has odd length: {value}")
    }
    (0..value.len())
        .step_by(2)
        .map(|index| Ok(u8::from_str_radix(&value[index..index + 2], 16)?))
        .collect()
}

fn update_hash(hash: &mut u64, bytes: impl IntoIterator<Item = u8>) {
    for byte in bytes {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
}

fn snapshot_hash(snapshot: &TerminalSnapshot) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    update_hash(&mut hash, snapshot.columns.to_le_bytes());
    update_hash(&mut hash, snapshot.rows.to_le_bytes());
    update_hash(&mut hash, snapshot.cursor.column.to_le_bytes());
    update_hash(&mut hash, snapshot.cursor.row.to_le_bytes());
    update_hash(&mut hash, [u8::from(snapshot.cursor_visible)]);
    update_hash(&mut hash, [snapshot.cursor_shape.renderer_code()]);
    update_hash(&mut hash, (snapshot.scrollback_rows as u64).to_le_bytes());
    for cell in &snapshot.cells {
        update_hash(&mut hash, (cell.character as u32).to_le_bytes());
        update_hash(&mut hash, [cell.combining_count()]);
        for combining in cell.combining_scalars() {
            update_hash(&mut hash, (combining as u32).to_le_bytes());
        }
        hash_color(&mut hash, cell.foreground);
        hash_color(&mut hash, cell.background);
        update_hash(
            &mut hash,
            [
                u8::from(cell.bold),
                u8::from(cell.underline),
                u8::from(cell.inverse),
            ],
        );
    }
    hash
}

fn hash_color(hash: &mut u64, color: Color) {
    match color {
        Color::Default => update_hash(hash, [0]),
        Color::Indexed(index) => update_hash(hash, [1, index]),
        Color::Rgb(red, green, blue) => update_hash(hash, [2, red, green, blue]),
    }
}

fn fixture_paths() -> Result<Vec<PathBuf>> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("teamy-terminal-core")
        .join("fixtures")
        .join("transcripts");
    let mut paths = fs::read_dir(&directory)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("txt"))
        .collect::<Vec<_>>();
    paths.sort();
    if paths.is_empty() {
        eyre::bail!("no replay fixtures found in {}", directory.display())
    }
    Ok(paths)
}

fn assert_expected_snapshot(path: &Path, snapshot: &TerminalSnapshot, expected: &Expected) {
    assert_eq!(
        Some(snapshot.columns),
        expected.columns,
        "{}",
        path.display()
    );
    assert_eq!(Some(snapshot.rows), expected.rows, "{}", path.display());
    assert_eq!(
        Some(snapshot.cursor.column),
        expected.cursor_column,
        "{}",
        path.display()
    );
    assert_eq!(
        Some(snapshot.cursor.row),
        expected.cursor_row,
        "{}",
        path.display()
    );
    assert_eq!(
        Some(snapshot.cursor_visible),
        expected.cursor_visible,
        "{}",
        path.display()
    );
    assert_eq!(
        Some(snapshot.scrollback_rows),
        expected.scrollback_rows,
        "{}",
        path.display()
    );
    assert_eq!(
        Some(snapshot_hash(snapshot)),
        expected.hash,
        "{}",
        path.display()
    );
}

#[test]
fn replay_fixtures_match_independent_snapshots() -> Result<()> {
    for path in fixture_paths()? {
        let contents = fs::read_to_string(&path)?;
        let fixture = parse_fixture(&path, &contents)?;
        let mut terminal = TerminalSession::new(
            TerminalLimits {
                max_columns: fixture.columns.max(200),
                max_rows: fixture.rows.max(40),
                max_scrollback_rows: fixture.max_scrollback_rows,
            },
            fixture.columns,
            fixture.rows,
        );
        let mut process_exit = None;
        let mut cancelled = false;
        for operation in fixture.operations {
            match operation {
                Operation::Write(bytes) => terminal.process_bytes(&bytes),
                Operation::Resize { columns, rows } => terminal.resize(columns, rows),
                Operation::ProcessExit { code, success } => process_exit = Some((code, success)),
                Operation::Cancelled => cancelled = true,
            }
        }

        let snapshot = terminal.snapshot();
        assert_expected_snapshot(&path, &snapshot, &fixture.expected);
        assert_eq!(
            process_exit,
            fixture.expected.process_exit,
            "{}",
            path.display()
        );
        if let Some(expected_cancelled) = fixture.expected.cancelled {
            assert_eq!(cancelled, expected_cancelled, "{}", path.display());
        }
    }
    Ok(())
}
