//! Built-in read-only preview: file metadata plus, for text files, the
//! first lines of content. Used when Neovim is unavailable, for non-text
//! files, and for directories. Never writes anything.

use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::bearings::pad_to_width;
use crate::fsops::{self, FsError};
use crate::i18n::{Lang, PreviewField, PreviewKind};

/// How many bytes are sniffed to decide text vs. binary.
pub const SNIFF_BYTES: usize = 8192;
/// Content cap for the built-in preview.
pub const MAX_PREVIEW_BYTES: u64 = 256 * 1024;
/// Line cap for the built-in preview.
pub const MAX_PREVIEW_LINES: usize = 500;
/// Content cap for the full-screen reader, which shows whole files
/// rather than a head.
pub const MAX_VIEW_BYTES: u64 = 1024 * 1024;
/// Line cap for the full-screen reader.
pub const MAX_VIEW_LINES: usize = 20_000;

/// A rendered preview: a title and plain text lines for the pager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewData {
    pub title: String,
    pub lines: Vec<String>,
}

/// What the full-screen reader found in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewSource {
    /// Not text: the reader refuses rather than painting control bytes.
    Binary,
    /// Readable text, already capped. `truncated` marks a file that had
    /// more to give than the caps allow.
    Text { text: String, truncated: bool },
}

/// Read a file for the full-screen reader: whole-file text up to
/// [`MAX_VIEW_BYTES`] and [`MAX_VIEW_LINES`], or [`ViewSource::Binary`].
/// Read-only, like everything else here.
pub fn read_view(path: &Path) -> Result<ViewSource, FsError> {
    let file = std::fs::File::open(path).map_err(|e| fsops::io_error(path, &e))?;
    let mut buf = Vec::new();
    file.take(MAX_VIEW_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| fsops::io_error(path, &e))?;
    let mut truncated = buf.len() as u64 > MAX_VIEW_BYTES;
    if truncated {
        buf.truncate(MAX_VIEW_BYTES as usize);
    }
    let head = &buf[..buf.len().min(SNIFF_BYTES)];
    if !buf.is_empty() && !is_probably_text(head) {
        return Ok(ViewSource::Binary);
    }
    let text = String::from_utf8_lossy(&buf);
    let text = if text.lines().count() > MAX_VIEW_LINES {
        truncated = true;
        text.lines()
            .take(MAX_VIEW_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text.into_owned()
    };
    Ok(ViewSource::Text { text, truncated })
}

/// Heuristic text detection: text contains no NUL bytes in the sample.
pub fn is_probably_text(sample: &[u8]) -> bool {
    !sample.contains(&0)
}

/// Read up to [`SNIFF_BYTES`] from a file for text detection.
pub fn sniff(path: &Path) -> Result<Vec<u8>, FsError> {
    let mut file = std::fs::File::open(path).map_err(|e| fsops::io_error(path, &e))?;
    let mut buf = vec![0u8; SNIFF_BYTES];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(fsops::io_error(path, &e)),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Build the built-in preview for any path: metadata header for
/// everything, followed by text content for readable text files.
///
/// The label column is measured, not counted: a Han label owns two cells
/// per character, so it is padded to [`Lang::preview_label_width`]
/// display columns and the values line up in either language.
pub fn build_preview(path: &Path, lang: Lang) -> Result<PreviewData, FsError> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| fsops::io_error(path, &e))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let label =
        |field: PreviewField| pad_to_width(lang.preview_label(field), lang.preview_label_width());
    let row = |field: PreviewField, value: &str| format!("{}{value}", label(field));

    let mut lines = vec![row(PreviewField::Path, &path.display().to_string())];

    let (kind, target_meta) = if meta.is_symlink() {
        let target = std::fs::read_link(path)
            .map(|t| t.display().to_string())
            .unwrap_or_else(|_| "?".to_string());
        lines.push(row(PreviewField::Symlink, &format!("-> {target}")));
        match std::fs::metadata(path) {
            Ok(t) => (
                if t.is_dir() {
                    PreviewKind::SymlinkDir
                } else if t.is_file() {
                    PreviewKind::SymlinkFile
                } else {
                    PreviewKind::SymlinkSpecial
                },
                Some(t),
            ),
            Err(_) => (PreviewKind::BrokenSymlink, None),
        }
    } else if meta.is_dir() {
        (PreviewKind::Directory, Some(meta.clone()))
    } else if meta.is_file() {
        (PreviewKind::RegularFile, Some(meta.clone()))
    } else {
        (PreviewKind::SpecialFile, Some(meta.clone()))
    };

    lines.push(row(PreviewField::Type, lang.preview_kind(kind)));
    if let Some(ref m) = target_meta {
        if m.is_file() {
            lines.push(row(
                PreviewField::Size,
                &lang.preview_size(&format_size(m.len()), m.len()),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            lines.push(row(
                PreviewField::Mode,
                &format_mode(m.permissions().mode()),
            ));
        }
        if let Ok(modified) = m.modified() {
            lines.push(row(
                PreviewField::Modified,
                &format_timestamp_in(modified, lang),
            ));
        }
    }

    match target_meta {
        Some(ref m) if m.is_dir() => {
            if let Ok(children) = std::fs::read_dir(path) {
                let count = children.count();
                lines.push(row(PreviewField::Entries, &count.to_string()));
            }
        }
        Some(ref m) if m.is_file() => {
            lines.push(String::new());
            let sample = sniff(path)?;
            if sample.is_empty() {
                lines.push(lang.empty_file().to_string());
            } else if is_probably_text(&sample) {
                lines.push(lang.preview_content_rule().to_string());
                append_text_preview(path, m.len(), &mut lines)?;
            } else {
                lines.push(lang.binary_not_shown().to_string());
            }
        }
        _ => {}
    }

    Ok(PreviewData { title: name, lines })
}

fn append_text_preview(path: &Path, len: u64, lines: &mut Vec<String>) -> Result<(), FsError> {
    let file = std::fs::File::open(path).map_err(|e| fsops::io_error(path, &e))?;
    let mut buf = Vec::new();
    file.take(MAX_PREVIEW_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| fsops::io_error(path, &e))?;
    let text = String::from_utf8_lossy(&buf);
    for (shown, line) in text.lines().enumerate() {
        if shown >= MAX_PREVIEW_LINES {
            break;
        }
        // Strip control characters so the pager can never be corrupted by
        // terminal escape sequences embedded in a file.
        let clean: String = line
            .chars()
            .map(|c| {
                if c.is_control() && c != '\t' {
                    '\u{FFFD}'
                } else {
                    c
                }
            })
            .collect();
        lines.push(clean);
    }
    let truncated = len > MAX_PREVIEW_BYTES || text.lines().count() > MAX_PREVIEW_LINES;
    if truncated {
        lines.push(format!(
            "--- truncated (showing at most {MAX_PREVIEW_LINES} lines / {} KiB) ---",
            MAX_PREVIEW_BYTES / 1024
        ));
    }
    Ok(())
}

/// Human-readable size: `973B`, `1.2K`, `4.0M`, `1.1G`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 10.0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// `ls -l` style mode string, e.g. `-rw-r--r--` or `drwxr-xr-x`.
#[cfg(unix)]
pub fn format_mode(mode: u32) -> String {
    let file_type = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        0o100000 => '-',
        0o060000 => 'b',
        0o020000 => 'c',
        0o010000 => 'p',
        0o140000 => 's',
        _ => '?',
    };
    let mut out = String::with_capacity(10);
    out.push(file_type);
    for shift in [6u32, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        out.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        out.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        out.push(if bits & 0o1 != 0 { 'x' } else { '-' });
    }
    out
}

/// Format a timestamp as `YYYY-MM-DD HH:MM UTC` without any date crate.
///
/// The stamp itself is digits and `UTC` in every language; only the one
/// case that has no stamp to show - a time before the epoch the clock is
/// counted from - is a phrase, and it is said in `lang`.
pub fn format_timestamp_in(time: SystemTime, lang: Lang) -> String {
    let Ok(since_epoch) = time.duration_since(UNIX_EPOCH) else {
        return lang.before_the_epoch().to_string();
    };
    let (year, month, day, hour, minute) = civil_from_duration(since_epoch);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

/// [`format_timestamp_in`] in English, for callers with no language of
/// their own to pass.
pub fn format_timestamp(time: SystemTime) -> String {
    format_timestamp_in(time, Lang::En)
}

/// Days-since-epoch to civil date (Howard Hinnant's algorithm), plus time
/// of day.
fn civil_from_duration(d: Duration) -> (i64, u32, u32, u32, u32) {
    let secs = d.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = (rem / 3600) as u32;
    let minute = ((rem % 3600) / 60) as u32;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day, hour, minute)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;
    use std::fs;

    #[test]
    fn text_detection() {
        assert!(is_probably_text(b"hello world\n"));
        assert!(is_probably_text("ünïcødé 檔案".as_bytes()));
        assert!(is_probably_text(b""));
        assert!(!is_probably_text(b"ELF\x00\x01\x02"));
    }

    #[test]
    fn preview_text_file_shows_metadata_and_content() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("näme with space.txt");
        fs::write(&file, "line one\nline two\n").unwrap();

        let preview = build_preview(&file, Lang::En).unwrap();
        assert_eq!(preview.title, "näme with space.txt");
        let text = preview.lines.join("\n");
        assert!(text.contains("regular file"));
        assert!(text.contains("line one"));
        assert!(text.contains("line two"));
        assert!(text.contains("modified"));
    }

    #[test]
    fn preview_binary_file_hides_content() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("blob.bin");
        fs::write(&file, [0u8, 159, 146, 150]).unwrap();

        let preview = build_preview(&file, Lang::En).unwrap();
        let text = preview.lines.join("\n");
        assert!(text.contains("binary file"));
        assert!(!text.contains("\u{0}"));
    }

    #[test]
    fn preview_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("empty");
        fs::write(&file, "").unwrap();
        let preview = build_preview(&file, Lang::En).unwrap();
        assert!(preview.lines.join("\n").contains("(empty file)"));
    }

    #[test]
    fn preview_directory_counts_entries() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a"), "").unwrap();
        fs::write(tmp.path().join("b"), "").unwrap();
        let preview = build_preview(tmp.path(), Lang::En).unwrap();
        let text = preview.lines.join("\n");
        assert!(text.contains("directory"));
        assert!(text.contains("entries   2"));
    }

    #[test]
    fn preview_missing_path_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(build_preview(&tmp.path().join("ghost"), Lang::En).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn preview_broken_symlink_reports_target() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("dangling");
        std::os::unix::fs::symlink("/nonexistent/target", &link).unwrap();
        let preview = build_preview(&link, Lang::En).unwrap();
        let text = preview.lines.join("\n");
        assert!(text.contains("broken symlink"));
        assert!(text.contains("/nonexistent/target"));
    }

    #[test]
    fn long_files_truncate() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("long.txt");
        let body: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        fs::write(&file, body).unwrap();
        let preview = build_preview(&file, Lang::En).unwrap();
        let text = preview.lines.join("\n");
        assert!(text.contains("truncated"));
        assert!(text.contains("line 499"));
        assert!(!text.contains("line 500\n"));
    }

    #[test]
    fn control_characters_are_neutralized() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("evil.txt");
        fs::write(&file, "safe\x1b[31mred\x07bell\tkeep-tab\n").unwrap();
        let preview = build_preview(&file, Lang::En).unwrap();
        let text = preview.lines.join("\n");
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\x07'));
        assert!(text.contains("keep-tab"));
    }

    #[test]
    fn size_formatting() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(973), "973B");
        assert_eq!(format_size(1024), "1.0K");
        assert_eq!(format_size(1536), "1.5K");
        assert_eq!(format_size(10 * 1024 * 1024), "10M");
        assert_eq!(format_size(1_181_116_006), "1.1G");
    }

    #[cfg(unix)]
    #[test]
    fn mode_formatting() {
        assert_eq!(format_mode(0o100644), "-rw-r--r--");
        assert_eq!(format_mode(0o040755), "drwxr-xr-x");
        assert_eq!(format_mode(0o120777), "lrwxrwxrwx");
    }

    #[test]
    fn timestamp_formatting_known_values() {
        assert_eq!(format_timestamp(UNIX_EPOCH), "1970-01-01 00:00 UTC");
        // 2000-01-01 00:00 UTC
        let t = UNIX_EPOCH + Duration::from_secs(946_684_800);
        assert_eq!(format_timestamp(t), "2000-01-01 00:00 UTC");
    }
}
