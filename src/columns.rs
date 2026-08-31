//! The listing's columns: which ones are drawn, how wide each one is,
//! and what each one says about an entry.
//!
//! The listing used to be one hard-coded shape - name, size, age - with
//! its arithmetic spread between a constant in `ui.rs` and the loop that
//! drew a row. This module is that shape made explicit and configurable,
//! and it keeps the same split the rest of Filecraft has: **everything
//! here is a total function of the listing snapshot, the language, and
//! the width available.** Nothing reads the filesystem, the clock, or
//! the terminal, so a screen's columns are asserted without a TTY.
//!
//! Three rules hold it together:
//!
//! - **The name column is the one that stretches.** Every other column
//!   has a width its language declares ([`Column::content_width`]), and
//!   the name gets what is left. A terminal too narrow for that drops
//!   whole columns by [`Column::drop_rank`] rather than squeezing the
//!   name into nothing - and it never drops [`Column::Name`] or
//!   [`Column::Size`], which are what a file listing is.
//! - **Every width is in display cells, never characters.** A Han
//!   character owns two cells, so `修改時間` is the same eight columns
//!   `MODIFIED` is and `種類` is four where `KIND` is four. A column is
//!   as wide as the wider of its header and its content *in the language
//!   being spoken*, so a translated header can never push a row past the
//!   border.
//! - **A header row is chrome.** It is drawn above the rows, it is never
//!   focusable, and it costs [`HEADER_ROWS`] rows of the listing - which
//!   is why [`crate::app::App::listing_rows`] subtracts it, exactly as
//!   the reader and the picker subtract their frames.

use std::time::SystemTime;

use crate::bearings::{display_width, pad_to_width_with, sanitize, Glyphs};
use crate::i18n::Lang;
use crate::nav::{Entry, EntryKind};
use crate::preview::{format_mode, format_size};

/// Rows the column header costs inside the listing area: the header
/// line and the rule under it.
///
/// This must match what [`crate::ui::draw_listing`] reserves, the same
/// coupling [`crate::pager::FRAME_ROWS`] and [`crate::picker::FRAME_ROWS`]
/// have: scrolling and drawing must agree about what a row is.
pub const HEADER_ROWS: usize = 2;

/// Columns every listing row spends on the cursor marker (`> ` or two
/// spaces), ahead of the first column.
pub const MARKER_COLS: usize = 2;

/// Cells between two columns.
pub const GAP_COLS: usize = 1;

/// The narrowest a name column is allowed to get before a lower-priority
/// column is dropped to buy it room. Below this a listing stops being a
/// listing: `readme…` names nothing.
pub const MIN_NAME_WIDTH: usize = 16;

/// One thing the listing can say about an entry.
///
/// A closed set on purpose: each variant is a column with a header, a
/// width, an alignment, and a rule for what it prints, all written out
/// per variant so a new column is a compile error everywhere it has not
/// been decided yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Column {
    /// File or folder name, with the `/ @ @!` kind markers. The only
    /// column that stretches, and the only one that cannot be turned off.
    Name,
    /// Human-readable size, or `<DIR>` for anything entered rather than
    /// read.
    Size,
    /// How long ago the entry was last written.
    Modified,
    /// How long ago the entry was created - macOS birth time, falling
    /// back to the modification time on a filesystem that has none.
    Created,
    /// What kind of document it is: `Directory`, `Markdown`, `Rust`.
    Kind,
    /// `ls -l` mode string, e.g. `-rw-r--r--`.
    Permissions,
    /// Owning user and group, by name where the system knows one.
    Owner,
}

impl Column {
    /// Every column, in the order the picker lists them and the order a
    /// bare `:columns name,size,...` is read in.
    pub const ALL: [Column; 7] = [
        Column::Name,
        Column::Size,
        Column::Modified,
        Column::Created,
        Column::Kind,
        Column::Permissions,
        Column::Owner,
    ];

    /// The name this column is written under in the config file and at
    /// the `:` prompt. Not localized: it is a key, like a flag name.
    pub fn code(self) -> &'static str {
        match self {
            Column::Name => "name",
            Column::Size => "size",
            Column::Modified => "modified",
            Column::Created => "created",
            Column::Kind => "kind",
            Column::Permissions => "permissions",
            Column::Owner => "owner",
        }
    }

    /// A column named by the user, at the prompt or in the config file.
    ///
    /// Generous about the spellings that can only mean one column -
    /// case, and the Finder wording the brief uses - because this is a
    /// value a person typed. It is not generous about abbreviations that
    /// could mean two things.
    pub fn parse(value: &str) -> Option<Column> {
        let normalized = value.trim().trim_matches('"').trim().to_ascii_lowercase();
        let normalized = normalized.replace([' ', '_'], "-");
        match normalized.as_str() {
            "name" | "filename" | "file-name" => Some(Column::Name),
            "size" => Some(Column::Size),
            "modified" | "date-modified" | "mtime" | "age" => Some(Column::Modified),
            "created" | "date-created" | "birth" | "btime" | "ctime" => Some(Column::Created),
            "kind" | "type" => Some(Column::Kind),
            "permissions" | "perms" | "mode" => Some(Column::Permissions),
            "owner" | "user" => Some(Column::Owner),
            _ => None,
        }
    }

    /// Which columns are given up first when the terminal is too narrow
    /// to hold them all, lowest number first.
    ///
    /// `None` means never: a listing without a name is not a listing,
    /// and a size is what tells a directory from a file at a glance -
    /// so the two survive every width and everything else is furniture.
    pub fn drop_rank(self) -> Option<u8> {
        match self {
            Column::Name | Column::Size => None,
            Column::Owner => Some(0),
            Column::Permissions => Some(1),
            Column::Created => Some(2),
            Column::Kind => Some(3),
            Column::Modified => Some(4),
        }
    }

    /// Whether a cell is flush left or flush right in its column. Sizes
    /// are read against each other, so they line up on the right; every
    /// other cell is text and reads from the left.
    pub fn align(self) -> Align {
        match self {
            Column::Size => Align::Right,
            _ => Align::Left,
        }
    }

    /// The column's header, in the language on screen.
    pub fn header(self, lang: Lang) -> &'static str {
        match self {
            Column::Name => lang.column_name(),
            Column::Size => lang.column_size(),
            Column::Modified => lang.column_modified(),
            Column::Created => lang.column_created(),
            Column::Kind => lang.column_kind(),
            Column::Permissions => lang.column_permissions(),
            Column::Owner => lang.column_owner(),
        }
    }

    /// Cells the widest cell this column can print occupies, in `lang`.
    ///
    /// [`Column::Name`] has none - it is the column that takes what is
    /// left - so this is what every *other* column costs, and it is
    /// measured rather than counted, because a Han kind word owns two
    /// cells per character.
    pub fn content_width(self, lang: Lang) -> usize {
        match self {
            // Never consulted: the name is the flexible column.
            Column::Name => MIN_NAME_WIDTH,
            // `1023B`, `4.2K`, `<DIR>`, with slack for the sizes a
            // sparse file can claim.
            Column::Size => 7,
            Column::Modified | Column::Created => lang.age_width(),
            Column::Kind => FileKind::ALL
                .iter()
                .map(|kind| display_width(kind.word(lang)))
                .max()
                .unwrap_or(0),
            // `-rwxr-xr-x` - the type character plus nine mode bits.
            Column::Permissions => 10,
            // `user:group`, long enough for the names a system hands out
            // and short enough that turning it on is not the whole row.
            Column::Owner => 16,
        }
    }

    /// Cells this column occupies on screen in `lang`: wide enough for
    /// its widest cell *and* for its header, so toggling the header row
    /// never reflows the listing under it.
    pub fn width(self, lang: Lang) -> usize {
        self.content_width(lang)
            .max(display_width(self.header(lang)))
    }

    /// What this column says about `entry`, unpadded.
    ///
    /// `now` is passed in rather than read, so a listing renders the
    /// same way in a test as it does on screen.
    pub fn cell(self, entry: &Entry, now: SystemTime, lang: Lang) -> String {
        match self {
            Column::Name => sanitize(&entry.display_name()),
            Column::Size => {
                if entry.is_parent || entry.is_enterable() {
                    lang.dir_marker().to_string()
                } else {
                    format_size(entry.size)
                }
            }
            Column::Modified => age_of(entry.modified, now, lang),
            // The brief's graceful fallback, and the reason it is here
            // rather than in `nav`: a filesystem with no birth time
            // still has a date to show, and deciding that purely is
            // what makes it testable.
            Column::Created => age_of(entry.created.or(entry.modified), now, lang),
            Column::Kind => FileKind::of(entry).word(lang).to_string(),
            Column::Permissions => entry.mode.map(format_mode).unwrap_or_default(),
            Column::Owner => match (&entry.owner, &entry.group) {
                (Some(owner), Some(group)) => sanitize(&format!("{owner}:{group}")),
                (Some(owner), None) => sanitize(owner),
                _ => String::new(),
            },
        }
    }
}

/// Which edge a cell is flush against inside its column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

fn age_of(at: Option<SystemTime>, now: SystemTime, lang: Lang) -> String {
    at.map(|when| crate::bearings::relative_time(now, when, lang))
        .unwrap_or_default()
}

/// What kind of document an entry is, as the `kind` column names it.
///
/// Extension-driven and deliberately so: a listing must not open every
/// file in a directory to label it, so an unknown extension is
/// [`FileKind::Data`] rather than a guess paid for with I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Directory,
    Symlink,
    BrokenLink,
    Special,
    Markdown,
    Text,
    Pdf,
    Rust,
    Toml,
    Json,
    Yaml,
    Html,
    Css,
    JavaScript,
    TypeScript,
    Python,
    Shell,
    Image,
    Audio,
    Video,
    Archive,
    Binary,
    /// A regular file whose extension names nothing Filecraft knows.
    Data,
}

impl FileKind {
    /// Every kind, so the column's width can be measured over all of
    /// them rather than over the handful somebody listed.
    pub const ALL: [FileKind; 23] = [
        FileKind::Directory,
        FileKind::Symlink,
        FileKind::BrokenLink,
        FileKind::Special,
        FileKind::Markdown,
        FileKind::Text,
        FileKind::Pdf,
        FileKind::Rust,
        FileKind::Toml,
        FileKind::Json,
        FileKind::Yaml,
        FileKind::Html,
        FileKind::Css,
        FileKind::JavaScript,
        FileKind::TypeScript,
        FileKind::Python,
        FileKind::Shell,
        FileKind::Image,
        FileKind::Audio,
        FileKind::Video,
        FileKind::Archive,
        FileKind::Binary,
        FileKind::Data,
    ];

    /// The kind of an entry: what it *is* first, then what its extension
    /// says it holds.
    pub fn of(entry: &Entry) -> FileKind {
        match entry.kind {
            EntryKind::Dir | EntryKind::SymlinkDir => FileKind::Directory,
            EntryKind::SymlinkBroken => FileKind::BrokenLink,
            EntryKind::SymlinkFile => FileKind::Symlink,
            EntryKind::Other => FileKind::Special,
            EntryKind::File => FileKind::of_name(&entry.name),
        }
    }

    /// The kind a file name's extension names.
    pub fn of_name(name: &str) -> FileKind {
        let extension = name
            .rsplit_once('.')
            // A dotfile with no other dot (`.zshrc`) has no extension:
            // the whole name is the name.
            .filter(|(stem, _)| !stem.is_empty())
            .map(|(_, ext)| ext.to_ascii_lowercase());
        let Some(extension) = extension else {
            return FileKind::Data;
        };
        match extension.as_str() {
            "md" | "markdown" | "mdown" | "mkd" => FileKind::Markdown,
            "txt" | "text" | "log" | "rst" | "adoc" => FileKind::Text,
            "pdf" => FileKind::Pdf,
            "rs" => FileKind::Rust,
            "toml" => FileKind::Toml,
            "json" | "jsonc" | "ndjson" => FileKind::Json,
            "yaml" | "yml" => FileKind::Yaml,
            "html" | "htm" | "xhtml" => FileKind::Html,
            "css" | "scss" | "sass" | "less" => FileKind::Css,
            "js" | "mjs" | "cjs" | "jsx" => FileKind::JavaScript,
            "ts" | "mts" | "cts" | "tsx" => FileKind::TypeScript,
            "py" | "pyi" => FileKind::Python,
            "sh" | "bash" | "zsh" | "fish" | "command" => FileKind::Shell,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "svg" | "bmp" | "tiff" | "ico" => {
                FileKind::Image
            }
            "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "aiff" => FileKind::Audio,
            "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" => FileKind::Video,
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "dmg" => {
                FileKind::Archive
            }
            "o" | "a" | "so" | "dylib" | "bin" | "exe" | "wasm" | "class" | "pyc" => {
                FileKind::Binary
            }
            _ => FileKind::Data,
        }
    }

    fn is_desktop_candidate(self) -> bool {
        matches!(
            self,
            FileKind::Pdf | FileKind::Image | FileKind::Audio | FileKind::Video
        )
    }

    /// The kind's word in `lang`. Format and language names stay
    /// themselves in both - `Markdown` is a name, not a word.
    pub fn word(self, lang: Lang) -> &'static str {
        match self {
            FileKind::Directory => lang.filekind_directory(),
            FileKind::Symlink => lang.filekind_symlink(),
            FileKind::BrokenLink => lang.filekind_broken_link(),
            FileKind::Special => lang.filekind_special(),
            FileKind::Markdown => lang.filekind_markdown(),
            FileKind::Text => lang.filekind_text(),
            FileKind::Pdf => lang.filekind_pdf(),
            FileKind::Rust => lang.filekind_rust(),
            FileKind::Toml => lang.filekind_toml(),
            FileKind::Json => lang.filekind_json(),
            FileKind::Yaml => lang.filekind_yaml(),
            FileKind::Html => lang.filekind_html(),
            FileKind::Css => lang.filekind_css(),
            FileKind::JavaScript => lang.filekind_javascript(),
            FileKind::TypeScript => lang.filekind_typescript(),
            FileKind::Python => lang.filekind_python(),
            FileKind::Shell => lang.filekind_shell(),
            FileKind::Image => lang.filekind_image(),
            FileKind::Audio => lang.filekind_audio(),
            FileKind::Video => lang.filekind_video(),
            FileKind::Archive => lang.filekind_archive(),
            FileKind::Binary => lang.filekind_binary(),
            FileKind::Data => lang.filekind_data(),
        }
    }
}

/// Whether a name identifies a safe, non-text desktop format.
pub fn name_belongs_to_the_desktop(name: &str) -> bool {
    FileKind::of_name(name).is_desktop_candidate() && !name.to_ascii_lowercase().ends_with(".svg")
}

/// Why a written column list was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// A word that names no column.
    Unknown(String),
    /// A list with nothing in it.
    Empty,
}

impl SpecError {
    /// Why the list was refused, in `lang`. The word the user typed is
    /// quoted as typed; only the explanation around it is translated.
    pub fn message(&self, lang: Lang) -> String {
        let known = Column::ALL
            .iter()
            .map(|c| c.code())
            .collect::<Vec<_>>()
            .join(", ");
        match self {
            SpecError::Unknown(word) => lang.unknown_column(word, &known),
            SpecError::Empty => lang.empty_column_list(&known),
        }
    }
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(Lang::En))
    }
}

impl std::error::Error for SpecError {}

/// Which columns the listing shows, and whether it draws their header.
///
/// The invariant this type exists to hold is that [`Column::Name`] is
/// always in it: a listing of sizes with no names is not a file
/// listing, so a written list that leaves the name out gets it back at
/// the front rather than being refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSet {
    visible: Vec<Column>,
    /// Whether the column header row is drawn above the listing.
    pub header: bool,
}

impl Default for ColumnSet {
    /// The listing Filecraft has always drawn - name, size, age - now
    /// with a header naming those three. Every other column is one
    /// `:columns` away, and none is turned on behind the user's back.
    fn default() -> Self {
        ColumnSet {
            visible: vec![Column::Name, Column::Size, Column::Modified],
            header: true,
        }
    }
}

impl ColumnSet {
    /// A set from an ordered list, deduplicated, with the name column
    /// guaranteed present.
    pub fn new(columns: impl IntoIterator<Item = Column>, header: bool) -> ColumnSet {
        let mut visible: Vec<Column> = Vec::new();
        for column in columns {
            if !visible.contains(&column) {
                visible.push(column);
            }
        }
        if !visible.contains(&Column::Name) {
            visible.insert(0, Column::Name);
        }
        ColumnSet { visible, header }
    }

    /// The columns to draw, in order.
    pub fn visible(&self) -> &[Column] {
        &self.visible
    }

    pub fn contains(&self, column: Column) -> bool {
        self.visible.contains(&column)
    }

    /// Read a written list: `name,size,modified` or `name size modified`.
    ///
    /// Commas and whitespace both separate, because both are what people
    /// write. An empty list is refused rather than silently meaning the
    /// default - `:columns ,,` is a typo, not a request.
    pub fn parse_spec(spec: &str) -> Result<Vec<Column>, SpecError> {
        let mut columns = Vec::new();
        let mut named = false;
        for word in spec.split([',', ' ', '\t']) {
            let word = word.trim();
            if word.is_empty() {
                continue;
            }
            named = true;
            let column = Column::parse(word).ok_or_else(|| SpecError::Unknown(word.to_string()))?;
            if !columns.contains(&column) {
                columns.push(column);
            }
        }
        if !named {
            return Err(SpecError::Empty);
        }
        Ok(columns)
    }

    /// The list as it is written back to the config file and echoed at
    /// the prompt - the same syntax [`ColumnSet::parse_spec`] reads.
    pub fn spec(&self) -> String {
        self.visible
            .iter()
            .map(|c| c.code())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Turn a column on or off, keeping [`Column::ALL`]'s order for one
    /// being turned on so the picker cannot build a listing whose
    /// columns are in an order nobody chose.
    ///
    /// The name column is never removed; asking to remove it is a no-op
    /// that reports `false`.
    pub fn toggle(&mut self, column: Column) -> bool {
        if column == Column::Name {
            return false;
        }
        if let Some(at) = self.visible.iter().position(|c| *c == column) {
            self.visible.remove(at);
            return true;
        }
        let rank = |c: Column| {
            Column::ALL
                .iter()
                .position(|a| *a == c)
                .unwrap_or(usize::MAX)
        };
        let at = self
            .visible
            .iter()
            .position(|c| rank(*c) > rank(column))
            .unwrap_or(self.visible.len());
        self.visible.insert(at, column);
        true
    }
}

/// One column as it is actually drawn: which one, and the cells it got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub column: Column,
    pub width: usize,
}

/// Fit `set` into `content` cells - the listing width less the rail and
/// the cursor marker.
///
/// The name column takes what the others leave. When that is less than
/// [`MIN_NAME_WIDTH`], the lowest-priority column present is dropped and
/// the sum is taken again, until the name has room or nothing droppable
/// is left. [`Column::Name`] and [`Column::Size`] are never dropped, so
/// even an absurdly narrow terminal still lists files and their sizes -
/// the name simply gets whatever is left, down to nothing, and
/// [`crate::bearings::pad_to_width_with`] cuts it on a cell boundary.
pub fn layout(set: &ColumnSet, content: usize, lang: Lang) -> Vec<Placement> {
    let mut chosen: Vec<Column> = set.visible.clone();
    if !chosen.contains(&Column::Name) {
        chosen.insert(0, Column::Name);
    }
    loop {
        let fixed: usize = chosen
            .iter()
            .filter(|c| **c != Column::Name)
            .map(|c| c.width(lang))
            .sum();
        let gaps = chosen.len().saturating_sub(1) * GAP_COLS;
        let name = content.saturating_sub(fixed + gaps);
        if name >= MIN_NAME_WIDTH {
            return placements(&chosen, name, lang);
        }
        // Give up the least useful column and try the sum again.
        let victim = chosen
            .iter()
            .copied()
            .filter_map(|c| c.drop_rank().map(|rank| (rank, c)))
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, c)| c);
        match victim {
            Some(column) => chosen.retain(|c| *c != column),
            None => return placements(&chosen, name, lang),
        }
    }
}

fn placements(chosen: &[Column], name: usize, lang: Lang) -> Vec<Placement> {
    chosen
        .iter()
        .map(|column| Placement {
            column: *column,
            width: if *column == Column::Name {
                name
            } else {
                column.width(lang)
            },
        })
        .collect()
}

/// Cells a laid-out row occupies, marker included - what `ui` has to
/// have left after the rail.
pub fn row_width(placed: &[Placement]) -> usize {
    let widths: usize = placed.iter().map(|p| p.width).sum();
    MARKER_COLS + widths + placed.len().saturating_sub(1) * GAP_COLS
}

/// One entry as a row of cells: the marker, then every placed column,
/// padded to exactly [`row_width`] cells.
pub fn row(
    placed: &[Placement],
    entry: &Entry,
    selected: bool,
    now: SystemTime,
    lang: Lang,
    glyphs: &Glyphs,
) -> String {
    let marker = if selected { "> " } else { "  " };
    let cells: Vec<String> = placed
        .iter()
        .map(|p| fit(&p.column.cell(entry, now, lang), p, glyphs))
        .collect();
    format!("{marker}{}", cells.join(&" ".repeat(GAP_COLS)))
}

/// The header line: the same geometry a row has, with each column's
/// name where its cells are.
pub fn header_row(placed: &[Placement], lang: Lang, glyphs: &Glyphs) -> String {
    let cells: Vec<String> = placed
        .iter()
        .map(|p| fit(p.column.header(lang), p, glyphs))
        .collect();
    format!(
        "{}{}",
        " ".repeat(MARKER_COLS),
        cells.join(&" ".repeat(GAP_COLS))
    )
}

/// The rule under the header: a full-width line in the character set the
/// screen is drawing with, so it obeys `FILECRAFT_ASCII` like every
/// other border.
pub fn rule(width: usize, glyphs: &Glyphs) -> String {
    glyphs.rule.repeat(width)
}

fn fit(text: &str, placed: &Placement, glyphs: &Glyphs) -> String {
    let padded = pad_to_width_with(text, placed.width, glyphs.ellipsis);
    match placed.column.align() {
        Align::Left => padded,
        Align::Right => {
            let text_width = display_width(text);
            if text_width >= placed.width {
                return padded;
            }
            format!("{}{text}", " ".repeat(placed.width - text_width))
        }
    }
}

/// One row of the `:columns` picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerRow {
    /// A column that can be turned on or off. The name column is listed
    /// too - it is what the listing is - but it is always on.
    Column(Column),
    /// The header row itself, so everything `:columns` governs is in one
    /// place rather than split across two commands.
    Header,
}

/// The interactive `:columns` picker: a cursor over every column and the
/// header switch, with Space toggling what it is on.
///
/// It edits a copy. Nothing about the listing changes until the picker
/// is confirmed, so cancelling really does leave the screen as it was -
/// the same contract the folder picker has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnPicker {
    pub set: ColumnSet,
    pub cursor: usize,
}

impl ColumnPicker {
    /// Rows the picker's own frame costs inside the listing area: two
    /// border rows plus the note that says what Space does.
    pub const FRAME_ROWS: usize = 3;

    /// Open the picker over the set currently in force.
    pub fn open(set: ColumnSet) -> ColumnPicker {
        ColumnPicker { set, cursor: 0 }
    }

    /// Every row, in order.
    pub fn rows() -> Vec<PickerRow> {
        Column::ALL
            .iter()
            .map(|c| PickerRow::Column(*c))
            .chain(std::iter::once(PickerRow::Header))
            .collect()
    }

    pub fn len(&self) -> usize {
        Column::ALL.len() + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let last = self.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
    }

    pub fn cursor_to_start(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_to_end(&mut self) {
        self.cursor = self.len() - 1;
    }

    /// The row under the cursor.
    pub fn focused(&self) -> PickerRow {
        Self::rows()[self.cursor.min(self.len() - 1)]
    }

    /// Whether the focused row is currently on.
    pub fn is_on(&self, row: PickerRow) -> bool {
        match row {
            PickerRow::Column(column) => self.set.contains(column),
            PickerRow::Header => self.set.header,
        }
    }

    /// Space: turn the focused row on or off. Reports `false` when the
    /// row refused - only the name column ever does.
    pub fn toggle(&mut self) -> bool {
        match self.focused() {
            PickerRow::Column(column) => self.set.toggle(column),
            PickerRow::Header => {
                self.set.header = !self.set.header;
                true
            }
        }
    }

    /// How a row reads: its label, and its `[x]` / `[ ]` box.
    pub fn label(&self, row: PickerRow, lang: Lang) -> String {
        match row {
            PickerRow::Column(column) => {
                format!("{} ({})", column.header(lang), column.code())
            }
            PickerRow::Header => lang.column_header_row().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::EntryKind;
    use std::time::Duration;

    const NOW: SystemTime = SystemTime::UNIX_EPOCH;

    fn glyphs() -> Glyphs {
        Glyphs::UNICODE
    }

    fn file(name: &str) -> Entry {
        Entry {
            name: name.to_string(),
            kind: EntryKind::File,
            size: 1024,
            modified: Some(NOW),
            created: Some(NOW),
            mode: Some(0o100_644),
            owner: Some("hsuan".to_string()),
            group: Some("staff".to_string()),
            symlink_target: None,
            is_parent: false,
        }
    }

    fn folder(name: &str) -> Entry {
        Entry {
            kind: EntryKind::Dir,
            size: 0,
            mode: Some(0o040_755),
            ..file(name)
        }
    }

    #[test]
    fn every_column_round_trips_through_its_code() {
        for column in Column::ALL {
            assert_eq!(Column::parse(column.code()), Some(column));
        }
    }

    #[test]
    fn a_column_can_be_named_the_way_a_person_writes_it() {
        assert_eq!(Column::parse("  NAME "), Some(Column::Name));
        assert_eq!(Column::parse("Date Modified"), Some(Column::Modified));
        assert_eq!(Column::parse("date_created"), Some(Column::Created));
        assert_eq!(Column::parse("perms"), Some(Column::Permissions));
        assert_eq!(Column::parse("type"), Some(Column::Kind));
        assert_eq!(Column::parse("colour"), None);
    }

    #[test]
    fn a_spec_reads_commas_and_spaces_alike_and_drops_repeats() {
        assert_eq!(
            ColumnSet::parse_spec("name, size ,modified name").unwrap(),
            vec![Column::Name, Column::Size, Column::Modified]
        );
    }

    #[test]
    fn a_spec_naming_nothing_is_refused_rather_than_meaning_the_default() {
        assert_eq!(ColumnSet::parse_spec(" , , "), Err(SpecError::Empty));
        assert_eq!(
            ColumnSet::parse_spec("name,colour"),
            Err(SpecError::Unknown("colour".to_string()))
        );
    }

    #[test]
    fn a_set_without_a_name_column_gets_one_back() {
        let set = ColumnSet::new([Column::Size, Column::Kind], true);
        assert_eq!(
            set.visible(),
            [Column::Name, Column::Size, Column::Kind].as_slice()
        );
        assert_eq!(set.spec(), "name,size,kind");
    }

    #[test]
    fn the_name_column_can_never_be_toggled_off() {
        let mut set = ColumnSet::default();
        assert!(!set.toggle(Column::Name));
        assert!(set.contains(Column::Name));
    }

    #[test]
    fn toggling_on_inserts_in_the_canonical_order() {
        let mut set = ColumnSet::new([Column::Name, Column::Owner], true);
        assert!(set.toggle(Column::Size));
        assert!(set.toggle(Column::Kind));
        assert_eq!(set.spec(), "name,size,kind,owner");
        assert!(set.toggle(Column::Owner));
        assert_eq!(set.spec(), "name,size,kind");
    }

    #[test]
    fn the_default_is_the_listing_filecraft_already_drew_plus_its_header() {
        let set = ColumnSet::default();
        assert_eq!(set.spec(), "name,size,modified");
        assert!(set.header);
    }

    #[test]
    fn every_header_fits_the_column_its_language_reserves() {
        for lang in Lang::ALL {
            for column in Column::ALL {
                assert!(
                    display_width(column.header(lang)) <= column.width(lang),
                    "{}: header '{}' is {} cells, wider than the {} reserved",
                    lang.code(),
                    column.header(lang),
                    display_width(column.header(lang)),
                    column.width(lang)
                );
            }
        }
    }

    #[test]
    fn every_kind_word_fits_the_kind_column() {
        for lang in Lang::ALL {
            let width = Column::Kind.width(lang);
            for kind in FileKind::ALL {
                assert!(
                    display_width(kind.word(lang)) <= width,
                    "{}: '{}' is wider than the {width} the kind column has",
                    lang.code(),
                    kind.word(lang)
                );
            }
        }
    }

    #[test]
    fn a_wide_terminal_keeps_every_configured_column() {
        let set = ColumnSet::new(Column::ALL, true);
        let placed = layout(&set, 140, Lang::En);
        assert_eq!(placed.len(), Column::ALL.len());
        assert!(placed[0].width >= MIN_NAME_WIDTH);
    }

    #[test]
    fn a_narrow_terminal_drops_the_lowest_priority_columns_first() {
        let set = ColumnSet::new(Column::ALL, true);
        let shown = |width: usize| -> Vec<Column> {
            layout(&set, width, Lang::En)
                .iter()
                .map(|p| p.column)
                .collect()
        };
        // Wide: everything. Then owner goes, then permissions, then the
        // created date, then the kind - modified is the last to leave.
        let mut previous = shown(140);
        for width in [70, 60, 50, 40, 30, 20] {
            let now = shown(width);
            assert!(
                now.len() <= previous.len(),
                "{width} cells kept more columns than a wider screen did"
            );
            for column in &now {
                assert!(
                    previous.contains(column),
                    "{width} cells brought back a column a wider screen had dropped"
                );
            }
            previous = now;
        }
        assert_eq!(shown(20), vec![Column::Name, Column::Size]);
    }

    #[test]
    fn name_and_size_survive_a_terminal_too_narrow_for_anything() {
        let set = ColumnSet::new(Column::ALL, true);
        for width in [0usize, 1, 4, 8, 12] {
            let placed = layout(&set, width, Lang::En);
            let columns: Vec<Column> = placed.iter().map(|p| p.column).collect();
            assert_eq!(columns, vec![Column::Name, Column::Size], "at {width}");
        }
    }

    #[test]
    fn a_laid_out_row_is_exactly_as_wide_as_it_claims() {
        for lang in Lang::ALL {
            for width in 8..140usize {
                let set = ColumnSet::new(Column::ALL, true);
                let placed = layout(&set, width, lang);
                let claimed = row_width(&placed);
                let entry = file("a-rather-long-file-name-that-will-not-fit.markdown");
                let drawn = row(&placed, &entry, true, NOW, lang, &glyphs());
                assert_eq!(
                    display_width(&drawn),
                    claimed,
                    "{} at {width}: {drawn:?}",
                    lang.code()
                );
                let header = header_row(&placed, lang, &glyphs());
                assert_eq!(
                    display_width(&header),
                    claimed,
                    "{} header at {width}: {header:?}",
                    lang.code()
                );
            }
        }
    }

    #[test]
    fn a_cjk_name_keeps_the_row_exactly_as_wide() {
        let set = ColumnSet::new(Column::ALL, true);
        for lang in Lang::ALL {
            let placed = layout(&set, 96, lang);
            let entry = file("這是一個非常長的中文檔案名稱用來測試欄位寬度.md");
            let drawn = row(&placed, &entry, false, NOW, lang, &glyphs());
            assert_eq!(display_width(&drawn), row_width(&placed));
        }
    }

    #[test]
    fn a_directory_says_dir_where_a_file_says_its_size() {
        let placed = layout(&ColumnSet::default(), 60, Lang::En);
        let dir_row = row(
            &placed,
            &folder("projects"),
            false,
            NOW,
            Lang::En,
            &glyphs(),
        );
        assert!(dir_row.contains("<DIR>"), "{dir_row:?}");
        assert!(dir_row.contains("projects/"), "{dir_row:?}");
        let file_row = row(&placed, &file("notes.md"), false, NOW, Lang::En, &glyphs());
        assert!(file_row.contains("1.0K"), "{file_row:?}");
    }

    #[test]
    fn the_size_column_is_flush_right_and_the_name_flush_left() {
        let placed = layout(&ColumnSet::default(), 60, Lang::En);
        let drawn = row(&placed, &file("a.md"), false, NOW, Lang::En, &glyphs());
        assert!(drawn.starts_with("  a.md "), "{drawn:?}");
        // `1.0K` is four cells in a seven-cell column, pushed right.
        assert!(drawn.contains("   1.0K"), "{drawn:?}");
    }

    #[test]
    fn a_created_column_falls_back_to_the_modified_time() {
        let born = NOW - Duration::from_secs(0);
        let mut entry = file("note.md");
        entry.created = None;
        entry.modified = Some(born);
        let later = NOW + Duration::from_secs(3 * 86_400);
        assert_eq!(Column::Created.cell(&entry, later, Lang::En), "3d");
        // With a real birth time it is that, not the modification time.
        entry.created = Some(born - Duration::from_secs(0));
        entry.modified = Some(later);
        assert_eq!(Column::Created.cell(&entry, later, Lang::En), "3d");
        assert_eq!(Column::Modified.cell(&entry, later, Lang::En), "0s");
    }

    #[test]
    fn an_entry_with_no_timestamps_leaves_its_date_cells_blank() {
        let mut entry = file("note.md");
        entry.created = None;
        entry.modified = None;
        assert_eq!(Column::Created.cell(&entry, NOW, Lang::En), "");
        assert_eq!(Column::Modified.cell(&entry, NOW, Lang::En), "");
    }

    #[test]
    fn permissions_read_as_ls_does_and_are_blank_when_unknown() {
        let entry = file("note.md");
        assert_eq!(
            Column::Permissions.cell(&entry, NOW, Lang::En),
            "-rw-r--r--"
        );
        let mut unknown = entry.clone();
        unknown.mode = None;
        assert_eq!(Column::Permissions.cell(&unknown, NOW, Lang::En), "");
    }

    #[test]
    fn an_owner_is_user_and_group_and_degrades_to_what_is_known() {
        let entry = file("note.md");
        assert_eq!(Column::Owner.cell(&entry, NOW, Lang::En), "hsuan:staff");
        let mut no_group = entry.clone();
        no_group.group = None;
        assert_eq!(Column::Owner.cell(&no_group, NOW, Lang::En), "hsuan");
        let mut nothing = entry.clone();
        nothing.owner = None;
        assert_eq!(Column::Owner.cell(&nothing, NOW, Lang::En), "");
    }

    #[test]
    fn a_control_character_in_a_name_or_an_owner_never_reaches_the_row() {
        let mut entry = file("note\u{1b}[31m.md");
        entry.owner = Some("h\u{7}suan".to_string());
        let placed = layout(&ColumnSet::new(Column::ALL, true), 120, Lang::En);
        let drawn = row(&placed, &entry, false, NOW, Lang::En, &glyphs());
        assert!(!drawn.contains('\u{1b}'), "{drawn:?}");
        assert!(!drawn.contains('\u{7}'), "{drawn:?}");
    }

    #[test]
    fn a_kind_is_read_off_the_extension_without_opening_anything() {
        assert_eq!(FileKind::of_name("notes.md"), FileKind::Markdown);
        assert_eq!(FileKind::of_name("NOTES.MD"), FileKind::Markdown);
        assert_eq!(FileKind::of_name("main.rs"), FileKind::Rust);
        assert_eq!(FileKind::of_name("Cargo.toml"), FileKind::Toml);
        assert_eq!(FileKind::of_name("paper.pdf"), FileKind::Pdf);
        assert_eq!(FileKind::of_name("libfoo.dylib"), FileKind::Binary);
        assert_eq!(FileKind::of_name("clip.mov"), FileKind::Video);
        assert_eq!(FileKind::of_name("README"), FileKind::Data);
        // A dotfile's name is not an extension.
        assert_eq!(FileKind::of_name(".zshrc"), FileKind::Data);
        assert_eq!(FileKind::of_name(".config.toml"), FileKind::Toml);
    }

    #[test]
    fn desktop_candidates_and_safe_names_are_kept_distinct() {
        // The split `l` acts on. Everything a person reads stays with
        // the reader; the formats the reader could only paint as
        // mojibake go to the default application. `Data` - an extension
        // Filecraft does not know - is deliberately on the reader's
        // side, because only the file's own bytes can settle it.
        let desktop = [
            FileKind::Pdf,
            FileKind::Image,
            FileKind::Audio,
            FileKind::Video,
        ];
        for kind in FileKind::ALL {
            assert_eq!(
                kind.is_desktop_candidate(),
                desktop.contains(&kind),
                "{kind:?}"
            );
        }
        assert!(name_belongs_to_the_desktop("report.pdf"));
        assert!(name_belongs_to_the_desktop("shot.PNG"));
        assert!(!name_belongs_to_the_desktop("drawing.svg"));
        assert!(!name_belongs_to_the_desktop("pack.zip"));
        assert!(!name_belongs_to_the_desktop("program.bin"));
        assert!(!name_belongs_to_the_desktop("notes.md"));
        assert!(!name_belongs_to_the_desktop("main.rs"));
        assert!(!name_belongs_to_the_desktop("README"));
    }

    #[test]
    fn what_an_entry_is_outranks_what_its_name_says() {
        let mut link = file("notes.md");
        link.kind = EntryKind::SymlinkFile;
        assert_eq!(FileKind::of(&link), FileKind::Symlink);
        let mut broken = link.clone();
        broken.kind = EntryKind::SymlinkBroken;
        assert_eq!(FileKind::of(&broken), FileKind::BrokenLink);
        // A directory called `assets.md` is still a directory.
        assert_eq!(FileKind::of(&folder("assets.md")), FileKind::Directory);
    }

    #[test]
    fn the_rule_obeys_the_character_set_the_screen_draws_with() {
        assert_eq!(rule(4, &Glyphs::UNICODE), "────");
        assert_eq!(rule(4, &Glyphs::ASCII), "----");
        assert!(rule(9, &Glyphs::ASCII).is_ascii());
    }

    #[test]
    fn the_picker_lists_every_column_and_the_header_switch() {
        let rows = ColumnPicker::rows();
        assert_eq!(rows.len(), Column::ALL.len() + 1);
        assert_eq!(*rows.last().unwrap(), PickerRow::Header);
        for column in Column::ALL {
            assert!(rows.contains(&PickerRow::Column(column)));
        }
    }

    #[test]
    fn the_picker_toggles_the_focused_row_and_clamps_its_cursor() {
        let mut picker = ColumnPicker::open(ColumnSet::default());
        picker.move_cursor(-5);
        assert_eq!(picker.focused(), PickerRow::Column(Column::Name));
        // The name column is listed but never turns off.
        assert!(!picker.toggle());
        assert!(picker.is_on(PickerRow::Column(Column::Name)));

        picker.cursor_to_end();
        assert_eq!(picker.focused(), PickerRow::Header);
        assert!(picker.is_on(PickerRow::Header));
        assert!(picker.toggle());
        assert!(!picker.is_on(PickerRow::Header));

        picker.move_cursor(100);
        assert_eq!(picker.cursor, picker.len() - 1);
    }

    #[test]
    fn the_picker_edits_a_copy_so_cancelling_costs_nothing() {
        let original = ColumnSet::default();
        let mut picker = ColumnPicker::open(original.clone());
        picker.cursor = 6; // owner
        assert!(picker.toggle());
        assert!(picker.set.contains(Column::Owner));
        assert!(!original.contains(Column::Owner));
    }

    #[test]
    fn every_picker_label_names_the_column_and_the_word_to_type() {
        let picker = ColumnPicker::open(ColumnSet::default());
        for lang in Lang::ALL {
            for row in ColumnPicker::rows() {
                let label = picker.label(row, lang);
                assert!(!label.trim().is_empty());
                if let PickerRow::Column(column) = row {
                    assert!(label.contains(column.code()), "{label:?}");
                }
            }
        }
    }
}
