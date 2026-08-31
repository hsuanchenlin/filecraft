//! Language: which one Filecraft speaks, and every phrase it says.
//!
//! One module owns both halves of localization, because they are the
//! same decision seen twice. [`Lang`] is *which* language - resolved
//! once at startup from the user's own setting and then from the system
//! locale, and switchable at runtime by `:lang`. Everything below it is
//! *what that language says*, as total functions of [`Lang`] and the
//! values a phrase interpolates.
//!
//! Three rules hold the module together:
//!
//! - **Nothing here reads the environment or the filesystem.**
//!   [`resolve`] is handed the strings it decides from, so language
//!   resolution is testable without setting a variable in the test
//!   process. `main.rs` reads the environment; [`crate::config`] reads
//!   the file.
//! - **Every phrase is a function of [`Lang`], and the compiler checks
//!   it.** A phrase is either in the [`phrases!`] table or a method that
//!   matches on `self`; a new language is a compile error in every place
//!   it has not been written yet, which is what keeps a screen from
//!   half-translating.
//! - **Every phrase is measured, never counted.** A Han character owns
//!   two terminal cells, so any phrase that lands in a padded column
//!   goes through [`crate::bearings::pad_to_width_with`] and any row
//!   that has to fit goes through [`crate::bearings::fit_joined`]. The
//!   fixed-width columns a translated phrase changes the size of -
//!   the listing's age field, the preview's label column - are
//!   [`Lang::age_width`] and [`Lang::preview_label_width`], so the
//!   arithmetic moves with the language instead of being pinned to
//!   English.
//!
//! Text Filecraft writes to a *file* is deliberately not localized: the
//! prompt handed to an AI provider, the session footer appended to a
//! summary, and the failure note that fills a reservation are artifacts
//! of a run rather than screen chrome, and they are read by tools and by
//! whoever the summary is shared with. The screen is localized; the
//! artifacts are stable.

/// A language Filecraft can speak.
///
/// Deliberately a closed set: every phrase in this module is written out
/// per variant, so a language exists exactly when it is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Lang {
    /// English. The default when nothing else resolves.
    #[default]
    En,
    /// Traditional Chinese (Taiwan) - 繁體中文.
    ZhTw,
}

impl Lang {
    /// Every language, in the order `:lang` lists them.
    pub const ALL: [Lang; 2] = [Lang::En, Lang::ZhTw];

    /// The canonical code: what is written to the config file and what
    /// `:lang` echoes back.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::ZhTw => "zh-TW",
        }
    }

    /// The language's name in itself, so the `:lang` listing is readable
    /// to a speaker of the language being offered.
    pub fn endonym(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::ZhTw => "繁體中文",
        }
    }

    /// A language named *explicitly* - by `:lang`, by `FILECRAFT_LANG`,
    /// or by the config file.
    ///
    /// Generous on purpose, because this is a value a person typed: case
    /// and the `-`/`_` split are ignored, and every spelling that can
    /// only mean one of the two languages is accepted. It is *not*
    /// generous about Simplified Chinese: `zh-CN` is a different written
    /// language from `zh-TW`, and answering it with Traditional
    /// characters would be a wrong answer rather than an approximate one.
    pub fn parse(value: &str) -> Option<Lang> {
        let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
        let normalized = normalized.trim_matches('"').trim();
        // A locale carries an encoding and a modifier the name does not.
        let bare = normalized
            .split(['.', '@'])
            .next()
            .unwrap_or(normalized)
            .trim();
        match bare {
            "" => None,
            "en" | "english" => Some(Lang::En),
            "zh-tw" | "zh-hant" | "zh-hant-tw" | "zh-hk" | "zh-mo" | "zhtw" | "tw"
            | "zh-hant-hk" | "繁體中文" | "正體中文" => Some(Lang::ZhTw),
            // Bare `zh` means the Chinese Filecraft has.
            "zh" | "chinese" => Some(Lang::ZhTw),
            other => match other.split('-').next() {
                // `en-US`, `en-GB`, and friends are all English.
                Some("en") => Some(Lang::En),
                _ => None,
            },
        }
    }

    /// The language a *system locale* asks for, if Filecraft speaks it.
    ///
    /// Stricter than [`Lang::parse`] in exactly one place: a bare `zh`
    /// region Filecraft does not have - `zh-CN`, `zh-SG`, `zh-Hans` -
    /// resolves to nothing rather than to Traditional Chinese, so a
    /// Simplified Chinese desktop gets English instead of the wrong
    /// Chinese. A user who wants Traditional anyway says so with
    /// `:lang zh`, which goes through [`Lang::parse`].
    pub fn from_locale(locale: &str) -> Option<Lang> {
        let normalized = locale.trim().to_ascii_lowercase().replace('_', "-");
        let bare = normalized.split(['.', '@']).next()?.trim();
        // The POSIX locales are "no preference", not a language.
        if bare.is_empty() || bare == "c" || bare == "posix" {
            return None;
        }
        let mut parts = bare.split('-');
        let language = parts.next()?;
        if language == "zh" {
            let tags: Vec<&str> = parts.collect();
            if tags.iter().any(|t| matches!(*t, "hans" | "cn" | "sg")) {
                return None;
            }
            return Some(Lang::ZhTw);
        }
        Lang::parse(language)
    }
}

/// Where a language came from, so `:lang` and a startup message can say
/// why the screen is in the language it is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `FILECRAFT_LANG`.
    Environment,
    /// `language = "..."` in the config file.
    Config,
    /// `LC_ALL` / `LC_MESSAGES` / `LANG`.
    Locale,
    /// Nothing asked, so English.
    Default,
}

/// Everything language resolution reads, gathered by the caller.
///
/// A plain struct of borrowed strings rather than a set of getters:
/// `main.rs` fills it from the real environment and the real config
/// file, and a test fills it from literals.
#[derive(Debug, Clone, Copy, Default)]
pub struct Request<'a> {
    /// `FILECRAFT_LANG`.
    pub env: Option<&'a str>,
    /// `language` as read from the config file.
    pub config: Option<&'a str>,
    /// `LC_ALL`.
    pub lc_all: Option<&'a str>,
    /// `LC_MESSAGES`.
    pub lc_messages: Option<&'a str>,
    /// `LANG`.
    pub lang: Option<&'a str>,
}

/// Resolve the language for this run.
///
/// The order is the one the user can predict: **what they said, then
/// what their system said, then English.** `FILECRAFT_LANG` beats the
/// config file because it is the more immediate of the two ways to say
/// a thing - one is this run, the other is every run - and among the
/// locale variables the order is POSIX's own (`LC_ALL` overrides
/// `LC_MESSAGES` overrides `LANG`).
///
/// A value that names no language Filecraft speaks is skipped rather
/// than fatal: a `LANG` of `fr_FR.UTF-8` falls through to English,
/// exactly as an unset one would.
pub fn resolve(request: &Request<'_>) -> (Lang, Source) {
    if let Some(lang) = request.env.and_then(Lang::parse) {
        return (lang, Source::Environment);
    }
    if let Some(lang) = request.config.and_then(Lang::parse) {
        return (lang, Source::Config);
    }
    for locale in [request.lc_all, request.lc_messages, request.lang] {
        if let Some(lang) = locale.and_then(Lang::from_locale) {
            return (lang, Source::Locale);
        }
    }
    (Lang::En, Source::Default)
}

/// One fixed phrase per language.
///
/// The table is the whole point: a phrase is one line, both languages
/// are on it, and adding a language is a compile error on every line
/// until it is written. Phrases that interpolate a value are methods
/// below instead, because a format string cannot be a table entry in
/// Rust.
macro_rules! phrases {
    ($($(#[$meta:meta])* $name:ident => $en:literal | $zh:literal),+ $(,)?) => {
        impl Lang {
            $(
                $(#[$meta])*
                pub fn $name(self) -> &'static str {
                    match self {
                        Lang::En => $en,
                        Lang::ZhTw => $zh,
                    }
                }
            )+

            /// Every fixed phrase in this language, with the name it is
            /// written under.
            ///
            /// The table's own index. It exists so a test can assert
            /// something about *all* of them - that none is empty, that
            /// none is wider than the row it lands on - rather than
            /// about the handful somebody remembered to list.
            pub fn phrases(self) -> Vec<(&'static str, &'static str)> {
                vec![$((stringify!($name), self.$name())),+]
            }
        }
    };
}

/// One fixed list of hints per language.
///
/// Hint rows are not translated word for word. The row is fitted by
/// dropping whole trailing hints ([`crate::bearings::fit_joined`]), so
/// each language orders its own by what a reader of that language needs
/// first, and a language whose words are wider simply shows fewer of
/// them on a narrow terminal - which is what English already does.
macro_rules! hint_rows {
    ($($(#[$meta:meta])* $name:ident => [$($en:literal),* $(,)?] | [$($zh:literal),* $(,)?]),+ $(,)?) => {
        impl Lang {
            $(
                $(#[$meta])*
                pub fn $name(self) -> &'static [&'static str] {
                    match self {
                        Lang::En => &[$($en),*],
                        Lang::ZhTw => &[$($zh),*],
                    }
                }
            )+

            /// Every hint row in this language, with the mode it belongs
            /// to - the same index [`Lang::phrases`] is, for the same
            /// reason.
            pub fn hint_rows(self) -> Vec<(&'static str, &'static [&'static str])> {
                vec![$((stringify!($name), self.$name())),+]
            }
        }
    };
}

phrases! {
    // ---- Browse screen -------------------------------------------------
    /// The note under a listing that a filter emptied entirely.
    no_matching_entries => "(no matching entries)" | "(無相符項目)",
    /// The status row when the listing shows nothing at all.
    no_rows => "no rows" | "無項目",
    /// The viewport's textual dual when the whole listing is on screen.
    all_rows_shown => "all rows shown" | "所有項目已顯示",
    /// Appended to the status row while `.` is in force.
    dotfiles_shown => "dotfiles shown" | "顯示隱藏檔",
    /// The size column for anything that is entered rather than read.
    /// A marker, not a word: it is the same four letters in every
    /// language, exactly as `/`, `@`, and `@!` are.
    dir_marker => "<DIR>" | "<DIR>",

    // ---- Listing column headers ----------------------------------------
    // Drawn above the rows and measured, never counted: `修改時間` is the
    // same eight cells `MODIFIED` is, because a Han character owns two.
    column_name => "NAME" | "名稱",
    column_size => "SIZE" | "大小",
    column_modified => "MODIFIED" | "修改時間",
    column_created => "CREATED" | "建立時間",
    column_kind => "KIND" | "種類",
    column_permissions => "PERMISSIONS" | "權限",
    column_owner => "OWNER" | "擁有者",
    /// The `:columns` picker's last row: the header switch itself, so
    /// everything the command governs is in one list.
    column_header_row => "column header row" | "欄位標題列",

    // ---- What kind of document a `kind` cell names ----------------------
    // A file format's own name is a marker, not a word: `Markdown` and
    // `PDF` are spelled the same in both languages, exactly as `<DIR>`
    // is. Everything that *is* a word is translated.
    filekind_directory => "Directory" | "目錄",
    filekind_symlink => "Symlink" | "符號連結",
    filekind_broken_link => "Broken" | "失效連結",
    filekind_special => "Special" | "特殊檔案",
    filekind_markdown => "Markdown" | "Markdown",
    filekind_text => "Text" | "文字",
    filekind_pdf => "PDF" | "PDF",
    filekind_rust => "Rust" | "Rust",
    filekind_toml => "TOML" | "TOML",
    filekind_json => "JSON" | "JSON",
    filekind_yaml => "YAML" | "YAML",
    filekind_html => "HTML" | "HTML",
    filekind_css => "CSS" | "CSS",
    filekind_javascript => "JavaScript" | "JavaScript",
    filekind_typescript => "TypeScript" | "TypeScript",
    filekind_python => "Python" | "Python",
    filekind_shell => "Shell" | "Shell",
    filekind_image => "Image" | "圖片",
    filekind_audio => "Audio" | "音訊",
    filekind_video => "Video" | "影片",
    filekind_archive => "Archive" | "壓縮檔",
    filekind_binary => "Binary" | "二進位",
    filekind_data => "Data" | "資料",

    // ---- Entry kinds, spoken -------------------------------------------
    kind_parent => "parent directory" | "上層目錄",
    kind_dir => "directory" | "目錄",
    kind_file => "file" | "檔案",
    kind_symlink_dir => "symlink to directory" | "目錄符號連結",
    kind_symlink_file => "symlink to file" | "檔案符號連結",
    kind_symlink_broken => "broken symlink" | "失效符號連結",
    kind_other => "special file" | "特殊檔案",

    // ---- Prompt row labels ---------------------------------------------
    prompt_command => " cmd> " | " 指令> ",
    prompt_filter => " filter> " | " 篩選> ",
    prompt_find => " find> " | " 搜尋: ",
    prompt_confirm => " confirm " | " 確認 ",
    prompt_yes_no => "[y]es / [n]o  " | "[y]是 / [n]否  ",
    prompt_read => " read " | " 閱讀模式 ",
    prompt_watch => " watch " | " 日誌檢視 ",
    prompt_pick => " pick " | " 選擇 ",
    prompt_browse_hint => "press : to type a command" | "按 : 輸入指令",
    /// The reader's one-line key reminder, on the prompt row.
    reader_keys => " j/k line {dot} d/u half {dot} g/G top/bottom {dot} / find {dot} h/q back"
        | " j/k 一行 {dot} d/u 半頁 {dot} g/G 首尾 {dot} / 搜尋 {dot} h/q 返回",
    /// The log viewer's second header half when no session was announced.
    no_session_reported => "no session reported" | "未回報工作階段",

    // ---- Overlay titles and footers ------------------------------------
    picker_title => " folder picker " | " 目錄選擇器 ",
    picker_keys => " j/k {dot} l in {dot} h up {dot} Enter/m select {dot} q cancel "
        | " j/k 瀏覽 {dot} l 進入 {dot} h 上層 {dot} Enter/m 選取 {dot} q 取消 ",
    selector_title => " summarize: pick files " | " 摘要：選擇檔案 ",
    selector_keys => " Space pick {dot} l in {dot} h up {dot} Enter/c confirm {dot} q cancel "
        | " Space 選取 {dot} l 進入 {dot} h 上層 {dot} Enter/c 確認 {dot} q 取消 ",
    columns_title => " listing columns " | " 列表欄位 ",
    columns_keys => " Space toggle {dot} j/k move {dot} Enter/c apply {dot} q cancel "
        | " Space 切換 {dot} j/k 移動 {dot} Enter/c 套用 {dot} q 取消 ",
    /// The `:columns` picker's note: what the list is actually for.
    /// Short enough to fit the popup at the documented 80x24 minimum.
    columns_picker_note => "name is always shown; a narrow terminal drops the rest from the bottom up"
        | "名稱欄位一律顯示；終端機太窄時會由下往上捨棄其餘欄位",
    provider_title => " summarize: pick a provider " | " 選擇 AI 模型 ",
    provider_keys => " 1-5 choose {dot} Enter default {dot} q cancel "
        | " 1-5 選擇 {dot} Enter 使用預設 (ag) {dot} q 取消 ",
    /// The provider dialog's closing note: what running one actually does.
    provider_scope_note => "the provider runs locally and reads only these files"
        | "AI 模型在本機執行，而且只會讀取這些檔案",
    /// How the default provider is marked in the dialog.
    provider_default_mark => "  [Default]" | "  [預設]",
    /// The reader pane's title when the app itself wrote the document.
    help_title => "help" | "說明",
    agent_title => "agent (not configured)" | "agent (未設定)",

    // ---- The AI summary run ---------------------------------------------
    activity_waiting => "waiting for output" | "等待輸出...",
    activity_thinking => "thinking" | "分析中...",
    activity_streaming => "streaming" | "輸出中...",
    activity_ended => "finished" | "完成",
    /// The log pane's note before a run has printed anything.
    no_output_yet => "(no output yet)" | "(尚無輸出)",
    /// The question the quit confirmation asks. One string, shared by
    /// the prompt row and the message log, so the two cannot drift.
    quit_question => "task in progress: terminate AI summary and quit?"
        | "背景任務執行中：確認終止 AI 摘要並離開？(y/n)",

    // ---- Reader / preview notes ------------------------------------------
    empty_file => "(empty file)" | "(空白檔案)",
    empty_directory => "(empty directory)" | "(空目錄)",
    binary_not_shown => "(binary file - content not shown)" | "(二進位檔案 - 不顯示內容)",
    preview_content_rule => "--- content ---" | "--- 內容 ---",
    no_messages_yet => "(no messages yet)" | "(尚無訊息)",

    // ---- Message-log lines ------------------------------------------------
    welcome => "welcome to filecraft - press ? for help, : for commands"
        | "歡迎使用 filecraft - 按 ? 顯示說明，按 : 輸入指令",
    filter_cleared => "filter cleared" | "已清除篩選",
    refreshed => "refreshed" | "已重新整理",
    dotfiles_now_shown => "dotfiles shown" | "已顯示隱藏檔",
    dotfiles_now_hidden => "dotfiles hidden" | "已隱藏隱藏檔",
    at_filesystem_root => "already at the filesystem root" | "已經在檔案系統根目錄",
    nothing_selected => "nothing selected" | "未選取任何項目",
    nothing_to_confirm => "nothing to confirm" | "沒有待確認的操作",
    nothing_focused => "nothing focused" | "游標下沒有項目",
    cannot_operate_on_parent => "cannot operate on '..' - select a real entry"
        | "無法對 '..' 執行操作 - 請選取實際項目",
    press_y_or_cancel => "press y to confirm, or n / q / Esc to cancel"
        | "按 y 確認，或按 n / q / Esc 取消",
    press_y_to_terminate => "press y to terminate the summary and quit, or n / Esc to stay"
        | "按 y 終止摘要並離開，或按 n / Esc 留下",
    summary_still_running => "cancelled: the summary is still running" | "已取消：摘要仍在執行",
    cancelled_folder_picker => "cancelled: folder picker" | "已取消：目錄選擇器",
    cancelled_summarize => "cancelled: summarize" | "已取消：AI 摘要",
    no_search_yet => "no search yet - press / to find" | "尚未搜尋 - 按 / 開始搜尋",
    press_l_to_read => "press l to read it" | "按 l 開始閱讀",
    watch_the_provider => "press L to watch what the provider is doing" | "按 L 觀看 AI 模型的即時輸出",
}

hint_rows! {
    /// Browse keys, ordered by how often they are needed: what falls off
    /// a narrow terminal is what the user needs least.
    hints_browse => [
        "j/k move", "l/Enter in", "h out", "0-9 jump", "/ find",
        ": cmd", "? help", "q quit", ". dotfiles", "M log",
    ] | [
        "j/k 移動", "l/Enter 進入", "h 上層", "0-9 跳轉", "/ 搜尋",
        ": 指令", "S AI摘要", "? 說明", "q 離開", ". 隱藏檔", "M 訊息",
    ],
    hints_command => [
        "Enter run", "Esc cancel", "try: help, cd, move, rename, preview",
    ] | [
        "Enter 執行", "Esc 取消", "可試: help, cd, move, rename, lang",
    ],
    hints_filter => [
        "type to filter", "Enter keep", "Esc clear",
    ] | [
        "輸入以篩選", "Enter 保留", "Esc 清除",
    ],
    hints_confirm_op => [
        "y confirm", "n/q/Esc cancel", "nothing happens without y",
    ] | [
        "y 確認", "n/q/Esc 取消", "沒有按 y 就不會有任何動作",
    ],
    hints_confirm_quit => [
        "y terminate and quit", "n/Esc keep running", "the summary keeps going without y",
    ] | [
        "y 終止並離開", "n/Esc 繼續執行", "沒有按 y 摘要就會繼續",
    ],
    hints_file_selector => [
        "Space pick", "j/k move", "l in", "h up", "Enter/c confirm", "q/Esc cancel",
    ] | [
        "Space 選取", "j/k 移動", "l 進入", "h 上層", "Enter/c 確認", "q/Esc 取消",
    ],
    hints_provider_menu => [
        "1-5 choose", "Enter default (ag)", "q/Esc cancel",
    ] | [
        "1-5 選擇", "Enter 使用預設 (ag)", "q/Esc 取消",
    ],
    hints_column_picker => [
        "Space toggle", "j/k move", "Enter/c apply", "q/Esc cancel", "name is always shown",
    ] | [
        "Space 切換", "j/k 移動", "Enter/c 套用", "q/Esc 取消", "名稱欄位一律顯示",
    ],
    hints_folder_picker => [
        "j/k focus", "l in", "h up", "Enter/m select", "q/Esc cancel",
    ] | [
        "j/k 移動", "l 進入", "h 上層", "Enter/m 選取", "q/Esc 取消",
    ],
    hints_pager_find => [
        "type to find", "Enter search", "Esc keep reading",
    ] | [
        "輸入以搜尋", "Enter 搜尋", "Esc 繼續閱讀",
    ],
    hints_pager => [
        "j/k line", "h/q/Esc back to files", "d/u half page", "/ find",
        "n/N next/prev", "PgUp/PgDn page",
    ] | [
        "j/k 一行", "h/q/Esc 返回列表", "d/u 半頁", "/ 搜尋",
        "n/N 下/上一個", "PgUp/PgDn 翻頁",
    ],
    hints_joblog_find => [
        "type to find", "Enter search", "Esc keep watching",
    ] | [
        "輸入以搜尋", "Enter 搜尋", "Esc 繼續觀看",
    ],
    hints_joblog_following => [
        "following new output", "h/q/Esc back to files", "j/k line", "d/u half page", "/ find",
    ] | [
        "自動跟隨新輸出", "h/q/Esc 返回列表", "j/k 一行", "d/u 半頁", "/ 搜尋",
    ],
    hints_joblog => [
        "j/k line", "h/q/Esc back to files", "G follow new output", "d/u half page", "/ find",
    ] | [
        "j/k 一行", "h/q/Esc 返回列表", "G 跟隨新輸出", "d/u 半頁", "/ 搜尋",
    ],
}

/// Fill the `{dot}` placeholder in a keys row with the separator the
/// screen is actually drawing with, so a keys row obeys `FILECRAFT_ASCII`
/// in every language.
pub fn keys_row(template: &str, dot: &str) -> String {
    template.replace("{dot}", dot)
}

impl Lang {
    // ---- Fixed-width columns a translation changes the size of ----------

    /// Columns the listing spends on an entry's age.
    ///
    /// A translated age is not the same width as an English one:
    /// `59m` is three columns and `59分鐘前` is eight, because a Han
    /// character owns two cells. The column moves with the language
    /// rather than the language being cut to fit the column - which is
    /// what would happen if this were a constant.
    pub fn age_width(self) -> usize {
        match self {
            Lang::En => 6,
            Lang::ZhTw => 8,
        }
    }

    /// Columns the built-in preview spends on its label column, measured
    /// the same way and for the same reason as [`Lang::age_width`].
    pub fn preview_label_width(self) -> usize {
        match self {
            Lang::En => 10,
            Lang::ZhTw => 10,
        }
    }

    // ---- Bearings --------------------------------------------------------

    /// The ladder's textual dual: how deep and how big, in words.
    pub fn ladder_summary(self, depth: usize, items: usize, dot: &str) -> String {
        match self {
            Lang::En => {
                let unit = if items == 1 { "item" } else { "items" };
                format!("depth {depth} {dot} {items} {unit}")
            }
            Lang::ZhTw => format!("階層 {depth} {dot} {items} 個項目"),
        }
    }

    /// `row R of T` - where the cursor is in what the filter let through.
    pub fn row_of(self, row: usize, total: usize) -> String {
        match self {
            Lang::En => format!("row {row} of {total}"),
            Lang::ZhTw => format!("第 {row} 列，共 {total} 列"),
        }
    }

    /// The rail's textual dual when only part of the listing is on screen.
    pub fn rows_range(self, first: usize, last: usize, total: usize) -> String {
        match self {
            Lang::En => format!("rows {first}-{last} of {total}"),
            Lang::ZhTw => format!("第 {first}-{last} 列，共 {total} 列"),
        }
    }

    /// What an active filter is letting through.
    pub fn filter_summary(self, filter: &str, matched: usize, total: usize) -> String {
        match self {
            Lang::En => format!("filter '{filter}': {matched} of {total} match"),
            Lang::ZhTw => format!("篩選 '{filter}'：{matched} / {total} 相符"),
        }
    }

    /// The note under a listing whose filter matched nothing real.
    pub fn no_entries_match(self, filter: &str) -> String {
        match self {
            Lang::En => format!("(no entries match '{filter}')"),
            Lang::ZhTw => format!("(沒有項目符合 '{filter}')"),
        }
    }

    /// A compact age, at most [`Lang::age_width`] columns.
    ///
    /// English is a bare count and a unit letter, because the status row
    /// says "ago" once for it ([`Lang::age_phrase`]). Traditional
    /// Chinese carries `前` in the word itself, because `2秒` alone
    /// reads as a duration rather than as a moment in the past - so the
    /// listing column and the spoken row want the very same string.
    pub fn age(self, seconds: u64) -> String {
        const MINUTE: u64 = 60;
        const HOUR: u64 = 60 * MINUTE;
        const DAY: u64 = 24 * HOUR;
        const WEEK: u64 = 7 * DAY;
        const YEAR: u64 = 365 * DAY;
        let (count, en_unit, zh_unit) = if seconds < MINUTE {
            (seconds, "s", "秒")
        } else if seconds < HOUR {
            (seconds / MINUTE, "m", "分鐘")
        } else if seconds < DAY {
            (seconds / HOUR, "h", "小時")
        } else if seconds < WEEK {
            (seconds / DAY, "d", "天")
        } else if seconds < YEAR {
            (seconds / WEEK, "w", "週")
        } else {
            (seconds / YEAR, "y", "年")
        };
        match self {
            Lang::En => format!("{count}{en_unit}"),
            Lang::ZhTw => format!("{count}{zh_unit}前"),
        }
    }

    /// [`Lang::age`] as the status row speaks it. English adds the word
    /// the column has no room for; Traditional Chinese already said it.
    pub fn age_phrase(self, age: &str) -> String {
        match self {
            Lang::En => format!("{age} ago"),
            Lang::ZhTw => age.to_string(),
        }
    }

    // ---- `:columns` / `:header` -------------------------------------------

    /// The prompt row while the picker is open: the list as it stands in
    /// the copy being edited, in the syntax `:columns` is typed in.
    pub fn columns_prompt(self, spec: &str) -> String {
        match self {
            Lang::En => format!("columns: {spec}"),
            Lang::ZhTw => format!("欄位：{spec}"),
        }
    }

    /// What a `:columns <list>` reports once the listing has changed.
    pub fn columns_set(self, spec: &str) -> String {
        self.op_says(Op::Columns, &self.columns_set_detail(spec))
    }

    fn columns_set_detail(self, spec: &str) -> String {
        match self {
            Lang::En => format!("columns set to {spec}"),
            Lang::ZhTw => format!("欄位已設定為 {spec}"),
        }
    }

    /// Whether the column header row is being drawn.
    pub fn header_is(self, on: bool) -> String {
        match (self, on) {
            (Lang::En, true) => "column header row: on".to_string(),
            (Lang::En, false) => "column header row: off".to_string(),
            (Lang::ZhTw, true) => "欄位標題列：顯示".to_string(),
            (Lang::ZhTw, false) => "欄位標題列：隱藏".to_string(),
        }
    }

    /// A word at the prompt or in the config file that names no column.
    pub fn unknown_column(self, value: &str, known: &str) -> String {
        self.op_says(Op::Columns, &self.unknown_column_detail(value, known))
    }

    fn unknown_column_detail(self, value: &str, known: &str) -> String {
        match self {
            Lang::En => format!("unknown column '{value}' - try one of: {known}"),
            Lang::ZhTw => format!("不支援的欄位 '{value}' - 可用: {known}"),
        }
    }

    /// A list that named nothing at all. Refused rather than read as the
    /// default, because `:columns ,,` is a typo and not a request.
    pub fn empty_column_list(self, known: &str) -> String {
        self.op_says(Op::Columns, &self.empty_column_list_detail(known))
    }

    fn empty_column_list_detail(self, known: &str) -> String {
        match self {
            Lang::En => format!("no columns named - try one or more of: {known}"),
            Lang::ZhTw => format!("沒有指定任何欄位 - 可用: {known}"),
        }
    }

    /// A `:set` whose left-hand side is not a setting Filecraft has.
    pub fn unknown_setting(self, value: &str, known: &str) -> String {
        match self {
            Lang::En => format!("unknown setting '{value}' - try one of: {known}"),
            Lang::ZhTw => format!("不支援的設定 '{value}' - 可用: {known}"),
        }
    }

    /// A `header=` value that is neither on nor off.
    pub fn unknown_switch(self, value: &str) -> String {
        match self {
            Lang::En => format!("expected 'on' or 'off', got '{value}'"),
            Lang::ZhTw => format!("預期為 'on' 或 'off'，實際為 '{value}'"),
        }
    }

    /// A column choice written to the settings file.
    pub fn columns_saved(self, path: &str) -> String {
        self.op_says(Op::Columns, &self.language_saved_detail(path))
    }

    /// A column choice that could not be written down. The session still
    /// has it; the next one will not.
    pub fn columns_not_saved(self, error: &str) -> String {
        self.op_says(Op::Columns, &self.language_not_saved_detail(error))
    }

    /// The `:columns` picker's own note, once it has been applied.
    pub fn columns_picker_cancelled(self) -> &'static str {
        match self {
            Lang::En => "cancelled: columns unchanged",
            Lang::ZhTw => "已取消：欄位未變更",
        }
    }

    /// Space on the name row, which is the one row that never toggles.
    pub fn name_column_is_always_shown(self) -> &'static str {
        match self {
            Lang::En => "the name column is always shown",
            Lang::ZhTw => "名稱欄位一律顯示",
        }
    }

    // ---- Confirmations ---------------------------------------------------

    /// A pending move, as the confirmation names it.
    pub fn describe_move(self, src: &str, dst: &str) -> String {
        match self {
            Lang::En => format!("move '{src}' -> '{dst}'"),
            Lang::ZhTw => format!("移動 '{src}' -> '{dst}'"),
        }
    }

    /// A pending rename, as the confirmation names it.
    pub fn describe_rename(self, from: &str, to: &str) -> String {
        match self {
            Lang::En => format!("rename '{from}' -> '{to}'"),
            Lang::ZhTw => format!("重新命名 '{from}' -> '{to}'"),
        }
    }

    /// A pending move to the Trash, as the confirmation names it.
    pub fn describe_trash(self, name: &str) -> String {
        match self {
            Lang::En => format!("trash '{name}'"),
            Lang::ZhTw => format!("將 '{name}' 移至垃圾桶"),
        }
    }

    /// The message-log line that arms an operation: what it is, and that
    /// a letter is what answers it.
    pub fn confirm_line(self, description: &str) -> String {
        match self {
            Lang::En => format!("confirm: {description} (y/n)"),
            Lang::ZhTw => format!("確認{description} (y/n)"),
        }
    }

    /// The message-log line for an operation that was answered `n`.
    pub fn cancelled(self, description: &str) -> String {
        match self {
            Lang::En => format!("cancelled: {description}"),
            Lang::ZhTw => format!("已取消：{description}"),
        }
    }

    /// The quit confirmation's own log line, naming the run at stake.
    pub fn confirm_quit_line(self, status: &str) -> String {
        match self {
            Lang::En => format!("confirm: quit and terminate {status}"),
            Lang::ZhTw => format!("確認：離開並終止 {status}"),
        }
    }

    /// A completed move or rename.
    pub fn moved(self, src: &str, dst: &str) -> String {
        match self {
            Lang::En => format!("moved '{src}' -> '{dst}'"),
            Lang::ZhTw => format!("已移動 '{src}' -> '{dst}'"),
        }
    }

    /// A completed rename.
    pub fn renamed(self, src: &str, dst: &str) -> String {
        match self {
            Lang::En => format!("renamed '{src}' -> '{dst}'"),
            Lang::ZhTw => format!("已重新命名 '{src}' -> '{dst}'"),
        }
    }

    /// A completed move to the Trash, naming where it can be got back from.
    pub fn trashed(self, name: &str, destination: &str) -> String {
        match self {
            Lang::En => format!("trashed '{name}' -> {destination} (recoverable from there)"),
            Lang::ZhTw => format!("已將 '{name}' 移至 {destination} (可從該處還原)"),
        }
    }

    // ---- Navigation ------------------------------------------------------

    /// The current directory, as the message log names it.
    pub fn cwd_line(self, path: &str) -> String {
        match self {
            Lang::En => format!("cwd: {path}"),
            Lang::ZhTw => format!("目錄: {path}"),
        }
    }

    /// A digit that addresses no rung the ladder is drawing.
    pub fn no_such_rung(self, digit: u8) -> String {
        match self {
            Lang::En => format!("no ancestor '{digit}' on the ladder"),
            Lang::ZhTw => format!("階層上沒有 '{digit}' 這個上層目錄"),
        }
    }

    /// A digit that addresses the directory already being shown.
    pub fn already_at(self, label: &str) -> String {
        match self {
            Lang::En => format!("already at {label}"),
            Lang::ZhTw => format!("已經在 {label}"),
        }
    }

    /// `l` or Enter on something that is neither a directory nor a file.
    pub fn broken_symlink(self, name: &str) -> String {
        match self {
            Lang::En => format!("broken symlink: '{name}' points nowhere"),
            Lang::ZhTw => format!("失效的符號連結：'{name}' 沒有指向任何目標"),
        }
    }

    pub fn cannot_open_special(self, name: &str) -> String {
        match self {
            Lang::En => format!("cannot open special file '{name}'"),
            Lang::ZhTw => format!("無法開啟特殊檔案 '{name}'"),
        }
    }

    pub fn cannot_read_special(self, name: &str) -> String {
        match self {
            Lang::En => format!("cannot read special file '{name}'"),
            Lang::ZhTw => format!("無法讀取特殊檔案 '{name}'"),
        }
    }

    pub fn not_a_directory(self, name: &str) -> String {
        match self {
            Lang::En => format!("'{name}' is not a directory"),
            Lang::ZhTw => format!("'{name}' 不是目錄"),
        }
    }

    pub fn not_a_regular_file(self, name: &str) -> String {
        self.op_says(Op::Edit, &self.not_a_regular_file_detail(name))
    }

    fn not_a_regular_file_detail(self, name: &str) -> String {
        match self {
            Lang::En => format!("'{name}' is not a regular file"),
            Lang::ZhTw => format!("'{name}' 不是一般檔案"),
        }
    }

    /// The reader's note when a file was too long to read whole.
    pub fn truncated(self, lines: usize, kib: u64) -> String {
        match self {
            Lang::En => format!("(truncated at {lines} lines / {kib} KiB)"),
            Lang::ZhTw => format!("(已截斷於 {lines} 行 / {kib} KiB)"),
        }
    }

    // ---- Editors and external programs -------------------------------------

    pub fn unsafe_desktop_open(self, name: &str) -> String {
        match self {
            Lang::En => format!("refusing to launch a handler for '{name}' from browse mode"),
            Lang::ZhTw => format!("拒絕從瀏覽模式啟動 '{name}' 的處理程式"),
        }
    }

    /// What both `:open` and `l` on a file the reader cannot draw say.
    /// One phrase, because they are one operation.
    pub fn opening_with_default_app(self, name: &str) -> String {
        self.op_says(Op::Open, &self.opening_with_default_app_detail(name))
    }

    fn opening_with_default_app_detail(self, name: &str) -> String {
        match self {
            Lang::En => format!("handing '{name}' to the macOS default application"),
            Lang::ZhTw => format!("正將 '{name}' 交給預設程式"),
        }
    }

    pub fn opening_in_editor(self, name: &str, program: &str) -> String {
        self.op_says(Op::Edit, &self.opening_in_editor_detail(name, program))
    }

    fn opening_in_editor_detail(self, name: &str, program: &str) -> String {
        match self {
            Lang::En => format!("opening '{name}' in {program}"),
            Lang::ZhTw => format!("以 {program} 開啟 '{name}'"),
        }
    }

    pub fn opening_preview(self, name: &str) -> String {
        self.op_says(Op::Preview, &self.opening_preview_detail(name))
    }

    fn opening_preview_detail(self, name: &str) -> String {
        match self {
            Lang::En => format!("opening '{name}' read-only in nvim"),
            Lang::ZhTw => format!("以 nvim 唯讀開啟 '{name}'"),
        }
    }

    pub fn program_closed(self, program: &str) -> String {
        match self {
            Lang::En => format!("{program} closed"),
            Lang::ZhTw => format!("{program} 已關閉"),
        }
    }

    pub fn program_exited(self, program: &str, status: &str) -> String {
        match self {
            Lang::En => format!("{program} exited with {status}"),
            Lang::ZhTw => format!("{program} 結束，狀態為 {status}"),
        }
    }

    pub fn failed_to_run(self, program: &str, error: &str) -> String {
        match self {
            Lang::En => format!("failed to run '{program}': {error}"),
            Lang::ZhTw => format!("無法執行 '{program}'：{error}"),
        }
    }

    // ---- Reader, message ring, preview ------------------------------------

    /// The message ring's title, which counts what it is holding.
    pub fn messages_title(self, shown: usize, capacity: usize) -> String {
        match self {
            Lang::En => format!("messages ({shown} of {capacity})"),
            Lang::ZhTw => format!("訊息 ({shown} / {capacity})"),
        }
    }

    pub fn preview_title(self, name: &str) -> String {
        match self {
            Lang::En => format!("preview: {name}"),
            Lang::ZhTw => format!("預覽: {name}"),
        }
    }

    /// The reader's position footer: where the view sits, in words.
    pub fn reader_position(self, line: usize, total: usize, percent: usize, dot: &str) -> String {
        match self {
            Lang::En => format!("line {line} of {total} {dot} {percent}%"),
            Lang::ZhTw => format!("第 {line} 行，共 {total} 行 {dot} {percent}%"),
        }
    }

    /// A `/` search that found nothing.
    pub fn no_match_for(self, query: &str) -> String {
        match self {
            Lang::En => format!("no match for '{query}'"),
            Lang::ZhTw => format!("找不到符合 '{query}' 的內容"),
        }
    }

    /// The built-in preview's label column, already padded so a
    /// translated label lines its values up like an English one.
    pub fn preview_label(self, field: PreviewField) -> &'static str {
        match (self, field) {
            (Lang::En, PreviewField::Path) => "path",
            (Lang::En, PreviewField::Symlink) => "symlink",
            (Lang::En, PreviewField::Type) => "type",
            (Lang::En, PreviewField::Size) => "size",
            (Lang::En, PreviewField::Mode) => "mode",
            (Lang::En, PreviewField::Modified) => "modified",
            (Lang::En, PreviewField::Entries) => "entries",
            (Lang::ZhTw, PreviewField::Path) => "路徑",
            (Lang::ZhTw, PreviewField::Symlink) => "符號連結",
            (Lang::ZhTw, PreviewField::Type) => "類型",
            (Lang::ZhTw, PreviewField::Size) => "大小",
            (Lang::ZhTw, PreviewField::Mode) => "權限",
            (Lang::ZhTw, PreviewField::Modified) => "修改時間",
            (Lang::ZhTw, PreviewField::Entries) => "項目數",
        }
    }

    /// The word the preview uses for what a path turned out to be.
    pub fn preview_kind(self, kind: PreviewKind) -> &'static str {
        match (self, kind) {
            (Lang::En, PreviewKind::SymlinkDir) => "symlink to directory",
            (Lang::En, PreviewKind::SymlinkFile) => "symlink to file",
            (Lang::En, PreviewKind::SymlinkSpecial) => "symlink to special file",
            (Lang::En, PreviewKind::BrokenSymlink) => "broken symlink",
            (Lang::En, PreviewKind::Directory) => "directory",
            (Lang::En, PreviewKind::RegularFile) => "regular file",
            (Lang::En, PreviewKind::SpecialFile) => "special file",
            (Lang::ZhTw, PreviewKind::SymlinkDir) => "目錄符號連結",
            (Lang::ZhTw, PreviewKind::SymlinkFile) => "檔案符號連結",
            (Lang::ZhTw, PreviewKind::SymlinkSpecial) => "特殊檔案符號連結",
            (Lang::ZhTw, PreviewKind::BrokenSymlink) => "失效符號連結",
            (Lang::ZhTw, PreviewKind::Directory) => "目錄",
            (Lang::ZhTw, PreviewKind::RegularFile) => "一般檔案",
            (Lang::ZhTw, PreviewKind::SpecialFile) => "特殊檔案",
        }
    }

    /// The preview's size line: a readable size and the exact byte count.
    pub fn preview_size(self, human: &str, bytes: u64) -> String {
        match self {
            Lang::En => format!("{human} ({bytes} bytes)"),
            Lang::ZhTw => format!("{human} ({bytes} 位元組)"),
        }
    }
}

/// A row of the built-in preview's metadata block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewField {
    Path,
    Symlink,
    Type,
    Size,
    Mode,
    Modified,
    Entries,
}

/// What the built-in preview found a path to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    SymlinkDir,
    SymlinkFile,
    SymlinkSpecial,
    BrokenSymlink,
    Directory,
    RegularFile,
    SpecialFile,
}

/// The argument shape a command wanted, as a value rather than as a
/// sentence.
///
/// [`crate::command::ParseError`] is produced by a parser that has no
/// language: it says *which* usage line is wanted and lets the screen
/// say it in the language the screen is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usage {
    Cd,
    Move,
    Rename,
    Trash,
    Summarize,
    Log,
    Language,
    Columns,
    Header,
    Set,
    /// The command takes nothing at all, so there is nothing to explain.
    None,
}

impl Lang {
    // ---- Command parsing --------------------------------------------------

    pub fn empty_command(self) -> &'static str {
        match self {
            Lang::En => "empty command",
            Lang::ZhTw => "指令是空的",
        }
    }

    pub fn unknown_command(self, word: &str) -> String {
        match self {
            Lang::En => format!("unknown command '{word}' (try 'help')"),
            Lang::ZhTw => format!("未知的指令 '{word}' (可輸入 'help')"),
        }
    }

    pub fn unterminated_quote(self) -> &'static str {
        match self {
            Lang::En => "unterminated quote",
            Lang::ZhTw => "引號沒有結尾",
        }
    }

    pub fn trailing_escape(self) -> &'static str {
        match self {
            Lang::En => "trailing backslash",
            Lang::ZhTw => "結尾多了一個反斜線",
        }
    }

    /// The usage line for a command that was given the wrong arguments.
    pub fn usage_line(self, command: &str, usage: Usage) -> String {
        let tail = match (self, usage) {
            (_, Usage::None) => "",
            (Lang::En, Usage::Cd) => "[path]   (quote paths containing spaces)",
            (Lang::En, Usage::Move) => {
                "[destination]   (no path opens the folder picker; quote spaces)"
            }
            (Lang::En, Usage::Rename) => "<new-name>   (renames the selected entry; quote spaces)",
            (Lang::En, Usage::Trash) => {
                "  (moves the selected entry to the Trash; there is no path form)"
            }
            (Lang::En, Usage::Summarize) => {
                "  (opens the file selector; Space picks files, Enter confirms)"
            }
            (Lang::En, Usage::Log) => "  (opens the AI run's own output; there is no path form)",
            (Lang::En, Usage::Language) => "[en|zh]   (no code shows the current language)",
            (Lang::En, Usage::Columns) => {
                "[name,size,modified,created,kind,permissions,owner]   (no list opens the picker)"
            }
            (Lang::En, Usage::Header) => "on|off   (the column header row above the listing)",
            (Lang::En, Usage::Set) => "columns=<list> | header=on|off",
            (Lang::ZhTw, Usage::Cd) => "[路徑]   (路徑含空白請加引號)",
            (Lang::ZhTw, Usage::Move) => "[目標]   (不給路徑會開啟目錄選擇器；含空白請加引號)",
            (Lang::ZhTw, Usage::Rename) => "<新名稱>   (重新命名選取的項目；含空白請加引號)",
            (Lang::ZhTw, Usage::Trash) => "  (將選取的項目移至垃圾桶；沒有路徑形式)",
            (Lang::ZhTw, Usage::Summarize) => "  (開啟檔案選擇器；Space 選取檔案，Enter 確認)",
            (Lang::ZhTw, Usage::Log) => "  (開啟 AI 執行的即時輸出；沒有路徑形式)",
            (Lang::ZhTw, Usage::Language) => "[en|zh]   (不給代碼會顯示目前語言)",
            (Lang::ZhTw, Usage::Columns) => {
                "[name,size,modified,created,kind,permissions,owner]   (不給清單會開啟選擇器)"
            }
            (Lang::ZhTw, Usage::Header) => "on|off   (列表上方的欄位標題列)",
            (Lang::ZhTw, Usage::Set) => "columns=<清單> | header=on|off",
        };
        match self {
            Lang::En => format!("usage: {command} {tail}"),
            Lang::ZhTw => format!("用法: {command} {tail}"),
        }
    }

    // ---- `:lang` ----------------------------------------------------------

    /// What `:lang` with no argument reports.
    pub fn language_is(self, endonym: &str, code: &str) -> String {
        match self {
            Lang::En => format!("language: {endonym} ({code}) - try ':lang en' or ':lang zh'"),
            Lang::ZhTw => format!("語言：{endonym} ({code}) - 可輸入 ':lang en' 或 ':lang zh'"),
        }
    }

    /// What `:lang <code>` reports once the screen has switched.
    pub fn language_set(self, endonym: &str, code: &str) -> String {
        match self {
            Lang::En => format!("language set to {endonym} ({code})"),
            Lang::ZhTw => format!("語言已設定為 {endonym} ({code})"),
        }
    }

    /// A `:lang` argument naming no language Filecraft speaks.
    pub fn unknown_language(self, value: &str, codes: &str) -> String {
        self.op_says(Op::Language, &self.unknown_language_detail(value, codes))
    }

    fn unknown_language_detail(self, value: &str, codes: &str) -> String {
        match self {
            Lang::En => format!("unknown language '{value}' - try one of: {codes}"),
            Lang::ZhTw => format!("不支援的語言 '{value}' - 可用: {codes}"),
        }
    }

    /// The preference was changed but could not be written down.
    pub fn language_not_saved(self, error: &str) -> String {
        self.op_says(Op::Language, &self.language_not_saved_detail(error))
    }

    fn language_not_saved_detail(self, error: &str) -> String {
        match self {
            Lang::En => {
                format!("this session only - could not save the preference: {error}")
            }
            Lang::ZhTw => format!("僅套用於本次執行 - 無法儲存偏好設定：{error}"),
        }
    }

    /// The preference was written down, and where.
    pub fn language_saved(self, path: &str) -> String {
        self.op_says(Op::Language, &self.language_saved_detail(path))
    }

    fn language_saved_detail(self, path: &str) -> String {
        match self {
            Lang::En => format!("saved to {path}"),
            Lang::ZhTw => format!("已儲存至 {path}"),
        }
    }

    // ---- The AI summary flow ------------------------------------------------

    /// The live status the status row shows while a run is going.
    pub fn job_status(self, files: usize, program: &str) -> String {
        match self {
            Lang::En => {
                let unit = if files == 1 { "file" } else { "files" };
                format!("[AI: summarizing {files} {unit} with {program}]")
            }
            Lang::ZhTw => format!("[AI: 正在使用 {program} 摘要 {files} 個檔案]"),
        }
    }

    /// `S` pressed while a run is already going.
    pub fn already_running(self, status: &str) -> String {
        self.op_says(Op::Summarize, &self.already_running_detail(status))
    }

    fn already_running_detail(self, status: &str) -> String {
        match self {
            Lang::En => format!("already running {status}"),
            Lang::ZhTw => format!("已有執行中的工作 {status}"),
        }
    }

    /// The selector's opening line, naming what it will accept.
    pub fn summarize_opened(self, extensions: &str) -> String {
        self.op_says(Op::Summarize, &self.summarize_opened_detail(extensions))
    }

    fn summarize_opened_detail(self, extensions: &str) -> String {
        match self {
            Lang::En => {
                format!("Space selects, Enter or c confirms, Esc cancels ({extensions})")
            }
            Lang::ZhTw => {
                format!("Space 選取，Enter 或 c 確認，Esc 取消 ({extensions})")
            }
        }
    }

    pub fn file_selected(self, name: &str, total: usize) -> String {
        match self {
            Lang::En => format!("selected '{name}' ({total} total)"),
            Lang::ZhTw => format!("已選取 '{name}' (共 {total} 個)"),
        }
    }

    pub fn file_unselected(self, name: &str, total: usize) -> String {
        match self {
            Lang::En => format!("unselected '{name}' ({total} total)"),
            Lang::ZhTw => format!("已取消選取 '{name}' (共 {total} 個)"),
        }
    }

    /// Only files are summarizable; Space landed on a folder.
    pub fn only_files_selectable(self, extensions: &str) -> String {
        match self {
            Lang::En => format!("only files can be selected ({extensions} documents)"),
            Lang::ZhTw => format!("只能選取檔案 ({extensions} 文件)"),
        }
    }

    /// The selector's header: how many files are picked right now.
    pub fn selector_header(self, chosen: usize, extensions: &str) -> String {
        match (self, chosen) {
            (Lang::En, 0) => format!("selected: 0 files - Space to select ({extensions})"),
            (Lang::En, 1) => "selected: 1 file".to_string(),
            (Lang::En, n) => format!("selected: {n} files"),
            (Lang::ZhTw, 0) => format!("已選取: 0 個檔案 - 按 Space 選取 ({extensions})"),
            (Lang::ZhTw, n) => format!("已選取: {n} 個檔案"),
        }
    }

    /// The selector's prompt-row line, which says what Enter would do.
    pub fn selector_prompt(self, chosen: usize) -> String {
        match (self, chosen) {
            (Lang::En, 0) => "files to summarize - Space marks the focused file".to_string(),
            (Lang::En, 1) => "1 file marked - Enter or c to choose a provider".to_string(),
            (Lang::En, n) => format!("{n} files marked - Enter or c to choose a provider"),
            (Lang::ZhTw, 0) => "要摘要的檔案 - 按 Space 標記游標所在的檔案".to_string(),
            (Lang::ZhTw, n) => format!("已標記 {n} 個檔案 - 按 Enter 或 c 選擇 AI 模型"),
        }
    }

    /// The line between the selector and the provider dialog.
    pub fn choose_a_provider(self, files: usize) -> String {
        self.op_says(Op::Summarize, &self.choose_a_provider_detail(files))
    }

    fn choose_a_provider_detail(self, files: usize) -> String {
        match self {
            Lang::En => {
                let unit = if files == 1 { "file" } else { "files" };
                format!("{files} {unit} - choose a provider (Enter takes the default)")
            }
            Lang::ZhTw => format!("{files} 個檔案 - 請選擇 AI 模型 (Enter 使用預設)"),
        }
    }

    /// The provider dialog's own header.
    pub fn files_selected(self, files: usize) -> String {
        match self {
            Lang::En => {
                let unit = if files == 1 { "file" } else { "files" };
                format!("{files} {unit} selected")
            }
            Lang::ZhTw => format!("已選取 {files} 個檔案"),
        }
    }

    /// The provider dialog's prompt-row line.
    pub fn provider_prompt(self, files: usize, default_code: &str) -> String {
        match self {
            Lang::En => format!(
                "provider for {files} file{} - Enter takes {default_code}",
                if files == 1 { "" } else { "s" }
            ),
            // Reads on from the ` 選擇 ` tag the prompt row draws in
            // front of it, rather than repeating the verb after it.
            Lang::ZhTw => format!("AI 模型，共 {files} 個檔案 - Enter 使用 {default_code}"),
        }
    }

    /// A digit at the provider dialog that names no provider.
    pub fn no_such_provider(self, digit: char) -> String {
        self.op_says(Op::Summarize, &self.no_such_provider_detail(digit))
    }

    fn no_such_provider_detail(self, digit: char) -> String {
        match self {
            Lang::En => format!("no provider '{digit}' - press 1-5 or Enter"),
            Lang::ZhTw => format!("沒有 '{digit}' 這個 AI 模型 - 請按 1-5 或 Enter"),
        }
    }

    pub fn will_write(self, path: &str) -> String {
        self.op_says(Op::Summarize, &self.will_write_detail(path))
    }

    fn will_write_detail(self, path: &str) -> String {
        match self {
            Lang::En => format!("will write {path}"),
            Lang::ZhTw => format!("將寫入 {path}"),
        }
    }

    pub fn summary_written(self, path: &str) -> String {
        match self {
            Lang::En => format!("summary written to {path}"),
            Lang::ZhTw => format!("摘要已寫入 {path}"),
        }
    }

    pub fn listing_moved_to(self, path: &str) -> String {
        match self {
            Lang::En => format!("listing moved to {path}"),
            Lang::ZhTw => format!("列表已切換到 {path}"),
        }
    }

    /// Messages whose whole text is a prefix and a sentence. Written
    /// this way rather than as one string so the prefix and
    /// [`Lang::op_name`] can never drift apart.
    pub fn home_unknown(self) -> String {
        self.op_says(
            Op::Cd,
            match self {
                Lang::En => "home directory unknown",
                Lang::ZhTw => "無法判斷家目錄",
            },
        )
    }

    pub fn move_same_place(self) -> String {
        self.op_says(
            Op::Move,
            match self {
                Lang::En => "source and destination are the same",
                Lang::ZhTw => "來源與目標相同",
            },
        )
    }

    pub fn move_into_itself(self) -> String {
        self.op_says(
            Op::Move,
            match self {
                Lang::En => "cannot move a directory into itself",
                Lang::ZhTw => "無法將目錄移入自身",
            },
        )
    }

    pub fn rename_same_name(self) -> String {
        self.op_says(
            Op::Rename,
            match self {
                Lang::En => "that is already the current name",
                Lang::ZhTw => "這已經是目前的名稱",
            },
        )
    }

    pub fn open_macos_only(self) -> String {
        self.op_says(
            Op::Open,
            match self {
                Lang::En => "only supported on macOS (uses /usr/bin/open)",
                Lang::ZhTw => "僅支援 macOS (使用 /usr/bin/open)",
            },
        )
    }

    pub fn agent_disabled(self) -> String {
        self.op_says(
            Op::Agent,
            match self {
                Lang::En => "not configured in v0",
                Lang::ZhTw => "v0 尚未啟用",
            },
        )
    }

    pub fn summarize_nothing_selected(self) -> String {
        self.op_says(
            Op::Summarize,
            match self {
                Lang::En => "no files selected - press Space on a file first",
                Lang::ZhTw => "尚未選取檔案 - 請先在檔案上按 Space",
            },
        )
    }

    pub fn summarize_no_files(self) -> String {
        self.op_says(
            Op::Summarize,
            match self {
                Lang::En => "no files selected",
                Lang::ZhTw => "尚未選取檔案",
            },
        )
    }

    pub fn log_never_ran(self) -> String {
        self.op_says(
            Op::Log,
            match self {
                Lang::En => "no AI summary has run yet - press S to pick files for one",
                Lang::ZhTw => "尚未執行過 AI 摘要 - 按 S 選擇要摘要的檔案",
            },
        )
    }

    /// Anything the summarizer itself reports, under the one prefix
    /// every message about a summary carries.
    pub fn summarize_error(self, reason: &str) -> String {
        self.op_says(Op::Summarize, reason)
    }

    // ---- The log viewer -----------------------------------------------------

    pub fn job_log_title(self, program: &str) -> String {
        match self {
            Lang::En => format!("job log: {program}"),
            Lang::ZhTw => format!("日誌檢視: {program}"),
        }
    }

    /// The log header's first row: who is running, what it is doing, and
    /// how much it has said.
    pub fn log_header_activity(
        self,
        program: &str,
        activity: &str,
        lines: usize,
        dot: &str,
    ) -> String {
        match self {
            Lang::En => {
                let unit = if lines == 1 { "line" } else { "lines" };
                format!("{program} {dot} {activity} {dot} {lines} {unit}")
            }
            Lang::ZhTw => format!("{program} {dot} {activity} {dot} {lines} 行"),
        }
    }

    /// The log header's second row when a session was announced.
    pub fn log_header_session(self, id: &str, resume: &str, dot: &str) -> String {
        match self {
            Lang::En => format!("session {id} {dot} resume: {resume}"),
            Lang::ZhTw => format!("工作階段 {id} {dot} 續接: {resume}"),
        }
    }

    /// The log header's second row when none was.
    pub fn log_header_no_session(self, program: &str) -> String {
        match self {
            Lang::En => format!("session: not reported by {program}"),
            Lang::ZhTw => format!("工作階段：{program} 未回報"),
        }
    }

    /// The log viewer's prompt-row line, naming the session it is over.
    pub fn watch_session(self, id: &str) -> String {
        match self {
            Lang::En => format!("session {id}"),
            Lang::ZhTw => format!("工作階段 {id}"),
        }
    }

    /// The log's note when the buffer has forgotten the start of a run.
    pub fn lines_dropped(self, dropped: usize, kept: usize) -> String {
        match self {
            Lang::En => {
                let unit = if dropped == 1 { "line" } else { "lines" };
                format!("({dropped} earlier {unit} dropped - the log keeps the last {kept})")
            }
            Lang::ZhTw => format!("(較早的 {dropped} 行已捨棄 - 日誌只保留最後 {kept} 行)"),
        }
    }

    // ---- The folder picker ---------------------------------------------------

    /// The picker's dest header - the dual of the focused folder.
    pub fn destination_line(self, path: &str) -> String {
        match self {
            Lang::En => format!("dest: {path}"),
            Lang::ZhTw => format!("目標: {path}"),
        }
    }

    /// The picker's prompt-row line.
    pub fn moving_to(self, source: &str, destination: &str) -> String {
        match self {
            Lang::En => format!("moving '{source}' -> '{destination}'"),
            Lang::ZhTw => format!("移動 '{source}' -> '{destination}'"),
        }
    }

    // ---- The static, TTY-free listing -----------------------------------------

    pub fn static_listing_note(self) -> &'static str {
        match self {
            Lang::En => {
                "static listing (no TTY). run in a real terminal for the interactive BBS screen."
            }
            Lang::ZhTw => "靜態列表 (沒有 TTY)。請在真正的終端機執行以取得互動式 BBS 畫面。",
        }
    }

    pub fn static_listing_keys(self) -> &'static str {
        match self {
            Lang::En => {
                "keys: j/k move  Enter/l open  Backspace/h up  / filter  : cmd  ? help  q quit"
            }
            Lang::ZhTw => {
                "按鍵: j/k 移動  Enter/l 開啟  Backspace/h 上層  / 篩選  : 指令  ? 說明  q 離開"
            }
        }
    }

    /// The warning `main.rs` prints before falling back to that listing.
    pub fn no_tty_warning(self) -> &'static str {
        match self {
            Lang::En => {
                "filecraft: no TTY detected; printing a static listing \
                 (run in a real terminal for the interactive screen)"
            }
            Lang::ZhTw => {
                "filecraft: 偵測不到 TTY；改為輸出靜態列表 (請在真正的終端機執行以取得互動式畫面)"
            }
        }
    }
}

impl Lang {
    // ---- Filesystem errors -----------------------------------------------
    //
    // [`crate::fsops::FsError`] is a value, not a sentence: it says what
    // went wrong and about which path, and the words come from here.

    pub fn fs_not_found(self, path: &str) -> String {
        match self {
            Lang::En => format!("not found: {path}"),
            Lang::ZhTw => format!("找不到: {path}"),
        }
    }

    pub fn fs_not_a_directory(self, path: &str) -> String {
        match self {
            Lang::En => format!("not a directory: {path}"),
            Lang::ZhTw => format!("不是目錄: {path}"),
        }
    }

    pub fn fs_not_a_file(self, path: &str) -> String {
        match self {
            Lang::En => format!("not a regular file: {path}"),
            Lang::ZhTw => format!("不是一般檔案: {path}"),
        }
    }

    pub fn fs_permission_denied(self, path: &str) -> String {
        match self {
            Lang::En => format!("permission denied: {path}"),
            Lang::ZhTw => format!("權限不足: {path}"),
        }
    }

    pub fn fs_already_exists(self, path: &str) -> String {
        match self {
            Lang::En => format!("destination already exists: {path}"),
            Lang::ZhTw => format!("目標已存在: {path}"),
        }
    }

    pub fn fs_cross_device(self) -> &'static str {
        match self {
            Lang::En => {
                "cross-volume move is not supported in v0; \
                 destination must be on the same volume"
            }
            Lang::ZhTw => "v0 不支援跨磁碟區移動；目標必須在同一個磁碟區",
        }
    }

    pub fn fs_invalid_name(self, name: &str, reason: &str) -> String {
        match self {
            Lang::En => format!("invalid name '{name}': {reason}"),
            Lang::ZhTw => format!("名稱不合法 '{name}'：{reason}"),
        }
    }

    pub fn fs_refused(self, path: &str, reason: &str) -> String {
        match self {
            Lang::En => format!("refused {path}: {reason}"),
            Lang::ZhTw => format!("已拒絕 {path}：{reason}"),
        }
    }

    pub fn fs_home_not_found(self) -> &'static str {
        match self {
            Lang::En => "cannot expand '~': home directory unknown",
            Lang::ZhTw => "無法展開 '~'：找不到家目錄",
        }
    }

    pub fn fs_io(self, message: &str, path: &str) -> String {
        match self {
            Lang::En => format!("{message}: {path}"),
            Lang::ZhTw => format!("{message}: {path}"),
        }
    }
}

/// Why a path or a name was refused.
///
/// A closed set rather than a sentence, for the same reason [`Usage`] is
/// one: the check that refuses a path has no language, and the screen
/// that reports it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// `~user` naming somebody else's home.
    TildeUser,
    EmptyPath,
    NoFileName,
    EmptyName,
    DotReserved,
    NameHasSlash,
    NameHasNul,
    /// The filesystem root is not something that can be trashed.
    RootNotTrashable,
    /// `..` is a row in the listing, but it is not an entry.
    ParentNotAnEntry,
    /// A move target with nothing above it to move into.
    NoParentDirectory,
    /// `.` likewise.
    CurrentNotAnEntry,
    /// Move-to-Trash has no implementation off macOS.
    TrashMacOsOnly,
}

impl Lang {
    /// The words for a [`Reason`].
    pub fn reason(self, reason: Reason) -> &'static str {
        match (self, reason) {
            (Lang::En, Reason::TildeUser) => "'~user' expansion is not supported",
            (Lang::En, Reason::EmptyPath) => "empty path",
            (Lang::En, Reason::NoFileName) => "destination has no file name",
            (Lang::En, Reason::EmptyName) => "empty name",
            (Lang::En, Reason::DotReserved) => "'.' and '..' are reserved",
            (Lang::En, Reason::NameHasSlash) => {
                "must not contain '/' (rename stays in the same directory)"
            }
            (Lang::En, Reason::NameHasNul) => "must not contain NUL",
            (Lang::En, Reason::RootNotTrashable) => "the filesystem root cannot be trashed",
            (Lang::En, Reason::ParentNotAnEntry) => {
                "'..' is the parent directory, not an entry - select a real entry"
            }
            (Lang::En, Reason::NoParentDirectory) => "destination has no parent directory",
            (Lang::En, Reason::CurrentNotAnEntry) => {
                "'.' is the current directory, not an entry - select a real entry"
            }
            (Lang::En, Reason::TrashMacOsOnly) => "moving to the Trash is only supported on macOS",
            (Lang::ZhTw, Reason::TildeUser) => "不支援 '~使用者' 形式的展開",
            (Lang::ZhTw, Reason::EmptyPath) => "路徑是空的",
            (Lang::ZhTw, Reason::NoFileName) => "目標沒有檔名",
            (Lang::ZhTw, Reason::EmptyName) => "名稱是空的",
            (Lang::ZhTw, Reason::DotReserved) => "'.' 與 '..' 是保留名稱",
            (Lang::ZhTw, Reason::NameHasSlash) => "不能包含 '/' (重新命名只會留在同一個目錄)",
            (Lang::ZhTw, Reason::NameHasNul) => "不能包含 NUL",
            (Lang::ZhTw, Reason::RootNotTrashable) => "檔案系統根目錄不能移至垃圾桶",
            (Lang::ZhTw, Reason::ParentNotAnEntry) => "'..' 是上層目錄，不是項目 - 請選取實際項目",
            (Lang::ZhTw, Reason::NoParentDirectory) => "目標沒有上層目錄",
            (Lang::ZhTw, Reason::CurrentNotAnEntry) => {
                "'.' 是目前的目錄，不是項目 - 請選取實際項目"
            }
            (Lang::ZhTw, Reason::TrashMacOsOnly) => "移至垃圾桶僅支援 macOS",
        }
    }

    /// Where a trashed entry went, as the confirmation names it.
    pub fn the_trash(self) -> &'static str {
        match self {
            Lang::En => "the Trash",
            Lang::ZhTw => "垃圾桶",
        }
    }
}

impl Lang {
    /// The full help text, shared by the `?` key and the `help` command.
    ///
    /// One written screen per language rather than a line-by-line
    /// translation of the English one. The key column is ASCII in both,
    /// so it lines up either way; what a line has room to say is not the
    /// same in two languages, and a help screen that reads like a
    /// translation is a help screen nobody finishes.
    pub fn help_lines(self) -> Vec<String> {
        let lines: &[&str] = match self {
            Lang::En => &[
                "FILECRAFT - keyboard-first BBS file navigator",
                "",
                "KEYS (browse)",
                "  j / k, Down / Up     move focus",
                "  PgUp / PgDn          move focus a page",
                "  g / G                first / last entry",
                "  Enter                enter directory, or edit selected file",
                "  l, Right             enter directory, or show the selected file",
                "                       (text in the reader; a PDF or image in macOS)",
                "  h, Left, Backspace   go to parent directory",
                "  0-9                  jump to that ancestor on the ladder",
                "  d                    move selected entry to the Trash (asks y/n)",
                "  S                    AI summary: pick files, then a provider",
                "  /                    filter the listing (Esc clears)",
                "  :                    command prompt",
                "  .                    show/hide dotfiles",
                "  r                    refresh listing",
                "  M                    message history",
                "  L                    live log of the AI run (also after it ends)",
                "  ?                    this help",
                "  Esc                  back out one level (clears a filter)",
                "  q, Ctrl-C            quit",
                "",
                "KEYS (reader - l on a text or Markdown file)",
                "  j / k, Down / Up     scroll one line",
                "  d / u                scroll half a page",
                "  f / b, PgDn / PgUp   scroll a page",
                "  g / G, Home / End    top / bottom",
                "  /                    find in this file (Enter searches)",
                "  n / N                next / previous match",
                "  h, q, Esc            back to the listing, on the same row",
                "",
                "KEYS (log viewer - L, :log or :job)",
                "  j / k, Down / Up     scroll one line",
                "  d / u                scroll half a page",
                "  f / b, PgDn / PgUp   scroll a page",
                "  g / G, Home / End    top / bottom",
                "  /                    find in the log (Enter searches)",
                "  n / N                next / previous match",
                "  h, q, Esc            back to the listing - the run keeps going",
                "  (new output follows the view while you are at the bottom;",
                "   scroll up to hold your place, G to follow again)",
                "  (NNN | is stdout, NNN ! is stderr; the header names the",
                "   session and the command that reopens it in the provider)",
                "",
                "KEYS (confirmation prompt)",
                "  y                    go ahead",
                "  Enter                go ahead - move and rename only, not trash",
                "  n, q, Esc            cancel - nothing is touched",
                "",
                "KEYS (file selector - S or :summarize)",
                "  j / k, Down / Up     move focus",
                "  PgUp / PgDn          move focus a page",
                "  Space                select / unselect the focused file",
                "  l, Right             enter the focused folder",
                "  h, Left, Backspace   go to parent directory",
                "  g / G                first / last row",
                "  Enter, c             confirm the selection, then pick a provider",
                "  q, Esc               cancel, back to the listing",
                "  (.pdf .md .markdown .txt only; the selection spans folders)",
                "",
                "KEYS (provider dialog)",
                "  1 - 5                run that provider",
                "  Enter                run the default, ag (agy)",
                "  q, Esc               cancel, nothing is run",
                "",
                "KEYS (quit with a summary running)",
                "  y                    terminate the summary and quit",
                "  n, Esc               keep it running, stay in filecraft",
                "",
                "KEYS (folder picker - :move with no path)",
                "  j / k, Down / Up     move focus",
                "  PgUp / PgDn          move focus a page",
                "  l, Right             enter the focused folder",
                "  h, Left, Backspace   go to parent directory",
                "  g / G                first / last folder",
                "  Enter, m             choose the focused folder (then y/n)",
                "  q, Esc               cancel, back to the listing",
                "",
                "KEYS (column picker - :columns with no list)",
                "  j / k, Down / Up     move focus",
                "  PgUp / PgDn          move focus a page",
                "  g / G                first / last row",
                "  Space                turn the focused column on or off",
                "  Enter, c             apply, and remember the choice",
                "  q, Esc               cancel, the listing is unchanged",
                "  (the name column is always shown; the last row is the",
                "   column header switch itself)",
                "",
                "COMMANDS (at the : prompt)",
                "  cd [path]            change directory (~ ok; quote spaces)",
                "  move [destination]   folder picker, or a path (asks y/n first)",
                "  rename <new-name>    rename selected entry (asks y/n first)",
                "  delete, trash        move selected entry to the Trash (asks y/n)",
                "  open                 open selected entry in its default app (same as l)",
                "  edit                 edit selected file in $EDITOR (or nvim)",
                "  preview              read-only preview (nvim -R, or built-in)",
                "  summarize, summary   AI summary of files you pick (same as S)",
                "  log, job             the AI run's own output (same as L)",
                "  agent [...]          future AI seam - disabled in v0",
                "  lang [en|zh]         screen language (saved for next time)",
                "  columns, cols [list] listing columns; no list opens the picker",
                "  header on|off        the column header row above the listing",
                "  set <key>=<value>    columns=<list> or header=on|off",
                "  help                 this help",
                "  quit                 leave filecraft",
                "",
                "COLUMNS  name size modified created kind permissions owner",
                "  - name is always shown and takes whatever width is left",
                "  - a narrow terminal drops columns from the bottom of that",
                "    list up; name and size are never dropped",
                "  - saved under [columns] in ~/.config/filecraft/config.toml",
                "",
                "SAFETY",
                "  - the reader is read-only: no key in it can change a file",
                "  - l on a PDF or image starts the macOS default application;",
                "    filecraft only names the file and never changes it",
                "  - moves and renames never overwrite and always ask first",
                "  - delete is a move to the Trash: recoverable, never an unlink",
                "  - nothing is ever removed permanently, recursively or otherwise",
                "  - commands are parsed directly; nothing touches a shell",
                "  - filecraft itself opens no network connection and keeps no",
                "    telemetry; 'summarize' runs an AI CLI you already have, on",
                "    files you picked, and that program may use the network",
                "  - a summary is never started, and no file is read for one,",
                "    until you select files and choose a provider",
                "  - the summary is a new .md file; it never overwrites one",
                "  - the log viewer only reads: closing it never stops a run,",
                "    and no key in it starts, resumes, or answers one",
                "  - the resume command is printed for you to run yourself;",
                "    filecraft never runs it",
                "",
                "MARKERS   name/ directory   name@ symlink   name@! broken symlink",
                "",
                "BEARINGS",
                "  - the ladder row is read-only: digits jump, nothing else acts there",
                "  - the rail column shows where the viewport sits in the listing",
                "  - the status row says the same thing in words, for speech",
                "",
                "press h, q, or Esc to close this help",
            ],
            Lang::ZhTw => &[
                "FILECRAFT - 鍵盤優先的 BBS 風格檔案瀏覽器",
                "",
                "按鍵 (瀏覽列表)",
                "  j / k, Down / Up     移動游標",
                "  PgUp / PgDn          上下翻頁",
                "  g / G                第一個 / 最後一個項目",
                "  Enter                進入目錄，或編輯選取的檔案",
                "  l, Right             進入目錄，或顯示選取的檔案",
                "                       (文字用閱讀模式；PDF、圖片交給預設程式)",
                "  h, Left, Backspace   回到上層目錄",
                "  0-9                  跳到階層上對應的上層目錄",
                "  d                    將選取的項目移至垃圾桶 (會先問 y/n)",
                "  S                    AI 摘要：先選檔案，再選 AI 模型",
                "  /                    篩選列表 (Esc 清除)",
                "  :                    指令提示列",
                "  .                    顯示 / 隱藏隱藏檔",
                "  r                    重新整理列表",
                "  M                    訊息紀錄",
                "  L                    AI 執行的即時日誌 (結束後仍可查看)",
                "  ?                    這份說明",
                "  Esc                  往回一層 (會清除篩選)",
                "  q, Ctrl-C            離開",
                "",
                "按鍵 (閱讀模式 - 在文字或 Markdown 檔案上按 l)",
                "  j / k, Down / Up     捲動一行",
                "  d / u                捲動半頁",
                "  f / b, PgDn / PgUp   捲動一頁",
                "  g / G, Home / End    最上方 / 最下方",
                "  /                    在這個檔案中搜尋 (Enter 開始搜尋)",
                "  n / N                下一個 / 上一個相符處",
                "  h, q, Esc            回到列表，游標停在原本那一列",
                "",
                "按鍵 (日誌檢視 - L、:log 或 :job)",
                "  j / k, Down / Up     捲動一行",
                "  d / u                捲動半頁",
                "  f / b, PgDn / PgUp   捲動一頁",
                "  g / G, Home / End    最上方 / 最下方",
                "  /                    在日誌中搜尋 (Enter 開始搜尋)",
                "  n / N                下一個 / 上一個相符處",
                "  h, q, Esc            回到列表 - 執行中的工作會繼續",
                "  (停在最下方時會自動跟隨新輸出；",
                "   往上捲動就會停住，按 G 重新跟隨)",
                "  (NNN | 是 stdout，NNN ! 是 stderr；標頭會顯示",
                "   工作階段，以及可在該 AI 模型中續接的指令)",
                "",
                "按鍵 (確認提示)",
                "  y                    執行",
                "  Enter                執行 - 僅限移動與重新命名，不含垃圾桶",
                "  n, q, Esc            取消 - 不會動到任何東西",
                "",
                "按鍵 (檔案選擇器 - S 或 :summarize)",
                "  j / k, Down / Up     移動游標",
                "  PgUp / PgDn          上下翻頁",
                "  Space                選取 / 取消選取游標所在的檔案",
                "  l, Right             進入游標所在的目錄",
                "  h, Left, Backspace   回到上層目錄",
                "  g / G                第一列 / 最後一列",
                "  Enter, c             確認選取，接著挑選 AI 模型",
                "  q, Esc               取消，回到列表",
                "  (僅限 .pdf .md .markdown .txt；選取可跨目錄)",
                "",
                "按鍵 (AI 模型對話框)",
                "  1 - 5                執行該 AI 模型",
                "  Enter                執行預設的 ag (agy)",
                "  q, Esc               取消，不會執行任何東西",
                "",
                "按鍵 (摘要執行中離開)",
                "  y                    終止摘要並離開",
                "  n, Esc               讓它繼續執行，留在 filecraft",
                "",
                "按鍵 (目錄選擇器 - :move 不帶路徑)",
                "  j / k, Down / Up     移動游標",
                "  PgUp / PgDn          上下翻頁",
                "  l, Right             進入游標所在的目錄",
                "  h, Left, Backspace   回到上層目錄",
                "  g / G                第一個 / 最後一個目錄",
                "  Enter, m             選定游標所在的目錄 (接著會問 y/n)",
                "  q, Esc               取消，回到列表",
                "",
                "按鍵 (欄位選擇器 - :columns 不帶清單)",
                "  j / k, Down / Up     移動游標",
                "  PgUp / PgDn          上下翻頁",
                "  g / G                第一列 / 最後一列",
                "  Space                切換游標所在欄位的顯示與否",
                "  Enter, c             套用，並記住這個選擇",
                "  q, Esc               取消，列表維持原狀",
                "  (名稱欄位一律顯示；最後一列是欄位標題列的開關)",
                "",
                "指令 (在 : 提示列輸入)",
                "  cd [路徑]            切換目錄 (可用 ~；含空白請加引號)",
                "  move [目標]          目錄選擇器，或直接給路徑 (會先問 y/n)",
                "  rename <新名稱>      重新命名選取的項目 (會先問 y/n)",
                "  delete, trash        將選取的項目移至垃圾桶 (會先問 y/n)",
                "  open                 以 macOS 預設程式開啟選取的項目 (同 l)",
                "  edit                 以 $EDITOR (或 nvim) 編輯選取的檔案",
                "  preview              唯讀預覽 (nvim -R，或內建預覽)",
                "  summarize, summary   對自選的檔案做 AI 摘要 (同 S)",
                "  log, job             AI 執行的即時輸出 (同 L)",
                "  agent [...]          未來的 AI 介面 - v0 尚未啟用",
                "  lang [en|zh]         畫面語言 (會記住下次使用)",
                "  columns, cols [清單] 列表欄位；不給清單會開啟選擇器",
                "  header on|off        列表上方的欄位標題列",
                "  set <鍵>=<值>        columns=<清單> 或 header=on|off",
                "  help                 這份說明",
                "  quit                 離開 filecraft",
                "",
                "欄位   name size modified created kind permissions owner",
                "  - name 一律顯示，並取用剩下的所有寬度",
                "  - 終端機太窄時，會由這份清單的尾端往前捨棄欄位；",
                "    name 與 size 永遠不會被捨棄",
                "  - 儲存在 ~/.config/filecraft/config.toml 的 [columns] 之下",
                "",
                "安全性",
                "  - 閱讀模式是唯讀的：其中任何按鍵都不會改動檔案",
                "  - 在 PDF 或圖片上按 l 會啟動 macOS 預設程式；",
                "    filecraft 只把檔案交出去，絕不會改動它",
                "  - 移動與重新命名絕不覆蓋既有檔案，而且一定會先詢問",
                "  - 刪除是移至垃圾桶：可以還原，絕不是 unlink",
                "  - 任何東西都不會被永久刪除，遞迴刪除也不會",
                "  - 指令由 filecraft 自行解析；完全不經過 shell",
                "  - filecraft 本身不會建立任何網路連線，也不收集遙測資料；",
                "    'summarize' 執行的是你電腦上既有的 AI CLI，對象是你選的",
                "    檔案，而該程式本身可能會使用網路",
                "  - 在你選好檔案並挑選 AI 模型之前，不會啟動任何摘要，",
                "    也不會為此讀取任何檔案",
                "  - 摘要是一個新的 .md 檔案；絕不會覆蓋既有檔案",
                "  - 日誌檢視只會讀取：關閉它不會停止執行中的工作，",
                "    其中也沒有任何按鍵會啟動、續接或回應工作",
                "  - 續接指令只會印出來給你自己執行；filecraft 絕不會代為執行",
                "",
                "標記   name/ 目錄   name@ 符號連結   name@! 失效的符號連結",
                "",
                "方位資訊",
                "  - 階層那一列是唯讀的：數字可跳轉，其餘按鍵在那裡不做任何事",
                "  - 側邊的位置軸顯示目前視窗落在整份列表的哪一段",
                "  - 狀態列用文字說同一件事，方便語音報讀",
                "",
                "按 h、q 或 Esc 關閉這份說明",
            ],
        };
        lines.iter().map(|s| s.to_string()).collect()
    }
}

/// An operation a message in the log is about.
///
/// The message log names the thing that produced each line. In English
/// that name happens to be the command word itself (`move:`, `delete:`);
/// in another language it is that language's word for the operation, so
/// a translated screen has no English left in it. What you *type* is
/// still the English command - the help screen's COMMANDS block is where
/// that is learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Cd,
    Move,
    Rename,
    Delete,
    Open,
    Edit,
    Preview,
    Read,
    Log,
    Summarize,
    Language,
    Columns,
    Agent,
}

impl Lang {
    /// What the message log calls an operation.
    pub fn op_name(self, op: Op) -> &'static str {
        match (self, op) {
            (Lang::En, Op::Cd) => "cd",
            (Lang::En, Op::Move) => "move",
            (Lang::En, Op::Rename) => "rename",
            (Lang::En, Op::Delete) => "delete",
            (Lang::En, Op::Open) => "open",
            (Lang::En, Op::Edit) => "edit",
            (Lang::En, Op::Preview) => "preview",
            (Lang::En, Op::Read) => "read",
            (Lang::En, Op::Log) => "log",
            (Lang::En, Op::Summarize) => "summarize",
            (Lang::En, Op::Language) => "lang",
            (Lang::En, Op::Columns) => "columns",
            (Lang::En, Op::Agent) => "agent",
            (Lang::ZhTw, Op::Cd) => "切換目錄",
            (Lang::ZhTw, Op::Move) => "移動",
            (Lang::ZhTw, Op::Rename) => "重新命名",
            (Lang::ZhTw, Op::Delete) => "刪除",
            (Lang::ZhTw, Op::Open) => "開啟",
            (Lang::ZhTw, Op::Edit) => "編輯",
            (Lang::ZhTw, Op::Preview) => "預覽",
            (Lang::ZhTw, Op::Read) => "閱讀",
            (Lang::ZhTw, Op::Log) => "日誌",
            (Lang::ZhTw, Op::Summarize) => "摘要",
            (Lang::ZhTw, Op::Language) => "語言",
            (Lang::ZhTw, Op::Columns) => "欄位",
            // The disabled AI seam is a name, not a word.
            (Lang::ZhTw, Op::Agent) => "agent",
        }
    }

    /// A message-log line named after the operation that produced it.
    pub fn op_says(self, op: Op, detail: &str) -> String {
        format!("{}: {detail}", self.op_name(op))
    }
}

/// Why argv did not parse.
///
/// A value rather than a sentence, like [`Usage`] and [`Reason`]: the
/// parser runs before anything on screen exists, and the words come from
/// the language that was resolved for this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    UnknownOption(String),
    TooManyDirectories,
    UnexpectedArgument(String),
}

impl CliError {
    /// The error in `lang`.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            CliError::UnknownOption(flag) => lang.cli_unknown_option(flag),
            CliError::TooManyDirectories => lang.cli_too_many_directories().to_string(),
            CliError::UnexpectedArgument(word) => lang.cli_unexpected_argument(word),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(Lang::En))
    }
}

impl std::error::Error for CliError {}

impl Lang {
    pub fn cli_unknown_option(self, flag: &str) -> String {
        match self {
            Lang::En => format!("unknown option '{flag}'"),
            Lang::ZhTw => format!("未知的選項 '{flag}'"),
        }
    }

    pub fn cli_too_many_directories(self) -> &'static str {
        match self {
            Lang::En => "expected at most one DIRECTORY argument",
            Lang::ZhTw => "最多只能指定一個 DIRECTORY 參數",
        }
    }

    pub fn cli_unexpected_argument(self, word: &str) -> String {
        match self {
            Lang::En => format!("unexpected argument '{word}'"),
            Lang::ZhTw => format!("多餘的參數 '{word}'"),
        }
    }

    /// What to try after argv failed to parse.
    pub fn cli_try_help(self) -> &'static str {
        match self {
            Lang::En => "try 'filecraft --help'",
            Lang::ZhTw => "可以試試 'filecraft --help'",
        }
    }

    /// `filecraft --help`.
    ///
    /// Option and command names are the tokens a shell actually accepts,
    /// so they are the same in every language; only the prose around
    /// them is translated.
    pub fn cli_usage(self) -> &'static str {
        match self {
            Lang::En => {
                "\
filecraft - keyboard-first, BBS-style terminal file navigator

USAGE:
  filecraft [OPTIONS] [DIRECTORY]
  filecraft update [--check]

OPTIONS:
  -l, --list       print a static listing and exit (no TUI)
  -h, --help       show this help
  -V, --version    show version

COMMANDS:
  update           install the latest filecraft
  update --check   report whether an update is available

No DIRECTORY opens the current working directory. A folder named
update is opened as `filecraft ./update`.

Interactive mode needs a real TTY. Without one, filecraft prints a
static listing of DIRECTORY (default: the current directory) instead.
Set NO_COLOR to disable colors; selection and markers stay visible.
Set FILECRAFT_ASCII to draw the screen in printable ASCII only.
Set FILECRAFT_LANG to en or zh-TW to choose the screen's language;
without it the system locale decides, and `:lang` changes it from
inside filecraft and remembers the choice.
"
            }
            Lang::ZhTw => {
                "\
filecraft - 鍵盤優先的 BBS 風格終端機檔案瀏覽器

用法:
  filecraft [選項] [目錄]
  filecraft update [--check]

選項:
  -l, --list       輸出靜態列表後結束 (不進入 TUI)
  -h, --help       顯示這份說明
  -V, --version    顯示版本

指令:
  update           安裝最新版的 filecraft
  update --check   檢查是否有可用的更新

不指定目錄時會開啟目前的工作目錄。名為 update 的資料夾請寫成
`filecraft ./update`。

互動模式需要真正的 TTY。沒有 TTY 時，filecraft 會改為輸出指定目錄
(預設為目前目錄) 的靜態列表。
設定 NO_COLOR 可關閉顏色；選取狀態與標記仍然看得見。
設定 FILECRAFT_ASCII 可讓畫面只用可列印的 ASCII 繪製。
設定 FILECRAFT_LANG 為 en 或 zh-TW 可指定畫面語言；沒有設定時由系統
語系決定，也可以在 filecraft 內用 `:lang` 切換並記住選擇。
"
            }
        }
    }

    /// `filecraft update --help`. The `cargo` and `git` command lines are
    /// what the user would type, so they are never translated.
    pub fn cli_update_usage(self) -> &'static str {
        match self {
            Lang::En => {
                "\
filecraft update - install the latest filecraft

USAGE:
  filecraft update [--check]

  --check    check for an update without installing

A local git clone is pulled with `git pull --ff-only` and reinstalled
with `cargo install --path <clone> --locked --force`. A global cargo
install is refreshed with:
  cargo install --git https://github.com/hsuanchenlin/filecraft.git --locked --force

Requires `cargo` (and `git` for a clone). Network, missing tools, and
permission errors are reported and do not crash.
"
            }
            Lang::ZhTw => {
                "\
filecraft update - 安裝最新版的 filecraft

用法:
  filecraft update [--check]

  --check    只檢查是否有更新，不進行安裝

本機的 git clone 會以 `git pull --ff-only` 更新，再用
`cargo install --path <clone> --locked --force` 重新安裝。全域的
cargo 安裝則以下列指令更新:
  cargo install --git https://github.com/hsuanchenlin/filecraft.git --locked --force

需要 `cargo` (使用 clone 時還需要 `git`)。網路問題、缺少工具與權限
錯誤都會被回報，不會讓程式當掉。
"
            }
        }
    }
}

impl Lang {
    // ---- What an AI summary run failed at ---------------------------------
    //
    // [`crate::summarize::Failure`] is a value like every other error in
    // the crate: the run says what went wrong, and the words come from
    // here. A provider's own last line travels inside it untranslated,
    // because that is evidence rather than prose.

    /// A run whose provider exited cleanly having produced nothing.
    pub fn provider_wrote_nothing(self) -> &'static str {
        match self {
            Lang::En => "the provider wrote nothing",
            Lang::ZhTw => "AI 模型沒有輸出任何內容",
        }
    }

    /// A run whose provider exited without a summary.
    pub fn provider_wrote_no_summary(self) -> &'static str {
        match self {
            Lang::En => "the provider exited without writing a summary",
            Lang::ZhTw => "AI 模型結束了，但沒有寫出摘要",
        }
    }

    /// A run answered `y` at the quit prompt.
    pub fn run_stopped(self) -> &'static str {
        match self {
            Lang::En => "the summary run was stopped before it could finish",
            Lang::ZhTw => "摘要在完成前就被終止了",
        }
    }

    /// A run whose worker went away without saying how it ended.
    pub fn run_without_result(self) -> &'static str {
        match self {
            Lang::En => "the summary run ended without a result",
            Lang::ZhTw => "摘要結束了，但沒有回報結果",
        }
    }

    /// The output file could not be claimed before the run started.
    pub fn could_not_reserve(self, path: &str, detail: &str) -> String {
        match self {
            Lang::En => format!("could not reserve {path}: {detail}"),
            Lang::ZhTw => format!("無法保留輸出檔 {path}：{detail}"),
        }
    }

    /// The provider could not be started at all.
    pub fn could_not_run(self, program: &str, detail: &str) -> String {
        match self {
            Lang::En => format!("could not run '{program}': {detail}"),
            Lang::ZhTw => format!("無法執行 '{program}'：{detail}"),
        }
    }

    /// The summary itself could not be written.
    pub fn could_not_write(self, path: &str, detail: &str) -> String {
        match self {
            Lang::En => format!("could not write {path}: {detail}"),
            Lang::ZhTw => format!("無法寫入 {path}：{detail}"),
        }
    }

    /// A failure that could not even be written down: what went wrong,
    /// and then what went wrong while recording it.
    pub fn could_not_record(self, reason: &str, path: Option<&str>, detail: &str) -> String {
        match (self, path) {
            (Lang::En, Some(path)) => {
                format!("{reason}; could not record failure in {path}: {detail}")
            }
            (Lang::En, None) => format!("{reason}; could not record failure: {detail}"),
            (Lang::ZhTw, Some(path)) => {
                format!("{reason}；也無法將失敗記錄到 {path}：{detail}")
            }
            (Lang::ZhTw, None) => format!("{reason}；也無法記錄這次失敗：{detail}"),
        }
    }

    /// A timestamp older than the epoch the clock is counted from.
    pub fn before_the_epoch(self) -> &'static str {
        match self {
            Lang::En => "before 1970",
            Lang::ZhTw => "1970 之前",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bearings::display_width;

    /// Every spelling a person might reasonably type for a language.
    #[test]
    fn an_explicitly_named_language_is_recognized_however_it_is_written() {
        for spelling in [
            "en",
            "EN",
            " en ",
            "english",
            "en-US",
            "en_GB",
            "en_US.UTF-8",
        ] {
            assert_eq!(Lang::parse(spelling), Some(Lang::En), "{spelling}");
        }
        for spelling in [
            "zh",
            "ZH",
            "zh-TW",
            "zh_TW",
            "zh_TW.UTF-8",
            "zh-Hant",
            "zh-Hant-TW",
            "zh_HK",
            "tw",
            "繁體中文",
        ] {
            assert_eq!(Lang::parse(spelling), Some(Lang::ZhTw), "{spelling}");
        }
    }

    #[test]
    fn a_language_filecraft_does_not_speak_names_nothing() {
        for spelling in ["", "  ", "fr", "ja", "de_DE.UTF-8", "klingon", "-"] {
            assert_eq!(Lang::parse(spelling), None, "{spelling}");
        }
    }

    #[test]
    fn a_traditional_chinese_locale_selects_traditional_chinese() {
        for locale in [
            "zh_TW.UTF-8",
            "zh_TW",
            "zh_HK.UTF-8",
            "zh-Hant",
            "zh_Hant_TW.UTF-8",
            "zh_MO",
            "zh",
        ] {
            assert_eq!(Lang::from_locale(locale), Some(Lang::ZhTw), "{locale}");
        }
    }

    #[test]
    fn a_simplified_chinese_locale_does_not_get_traditional_characters() {
        // Simplified is a different written language, not a near miss:
        // answering `zh_CN` with Traditional characters would be wrong
        // rather than approximate, so it falls through to English.
        for locale in ["zh_CN.UTF-8", "zh_SG", "zh-Hans", "zh_Hans_CN.UTF-8"] {
            assert_eq!(Lang::from_locale(locale), None, "{locale}");
        }
    }

    #[test]
    fn the_posix_locales_are_no_preference_rather_than_a_language() {
        for locale in ["C", "POSIX", "c.UTF-8", "", "  "] {
            assert_eq!(Lang::from_locale(locale), None, "{locale}");
        }
    }

    #[test]
    fn nothing_configured_anywhere_is_english() {
        assert_eq!(
            resolve(&Request::default()),
            (Lang::En, Source::Default),
            "English is the default, and it is reached by asking for nothing"
        );
    }

    #[test]
    fn the_environment_beats_the_config_file_and_the_config_file_beats_the_locale() {
        let request = Request {
            env: Some("en"),
            config: Some("zh-TW"),
            lc_all: Some("zh_TW.UTF-8"),
            ..Request::default()
        };
        assert_eq!(resolve(&request), (Lang::En, Source::Environment));

        let request = Request {
            config: Some("zh-TW"),
            lc_all: Some("en_US.UTF-8"),
            ..Request::default()
        };
        assert_eq!(resolve(&request), (Lang::ZhTw, Source::Config));

        let request = Request {
            lang: Some("zh_TW.UTF-8"),
            ..Request::default()
        };
        assert_eq!(resolve(&request), (Lang::ZhTw, Source::Locale));
    }

    #[test]
    fn the_locale_variables_are_read_in_posix_order() {
        let request = Request {
            lc_all: Some("en_US.UTF-8"),
            lc_messages: Some("zh_TW.UTF-8"),
            lang: Some("zh_TW.UTF-8"),
            ..Request::default()
        };
        assert_eq!(resolve(&request), (Lang::En, Source::Locale));

        let request = Request {
            lc_messages: Some("zh_TW.UTF-8"),
            lang: Some("en_US.UTF-8"),
            ..Request::default()
        };
        assert_eq!(resolve(&request), (Lang::ZhTw, Source::Locale));
    }

    #[test]
    fn a_value_naming_no_language_is_skipped_rather_than_fatal() {
        // A French desktop is not an error; it is a language filecraft
        // does not have, so the next answer down is used.
        let request = Request {
            env: Some("klingon"),
            config: Some("nonsense"),
            lc_all: Some("fr_FR.UTF-8"),
            lang: Some("zh_TW.UTF-8"),
            ..Request::default()
        };
        assert_eq!(resolve(&request), (Lang::ZhTw, Source::Locale));
    }

    #[test]
    fn a_code_round_trips_through_its_own_parser() {
        for lang in Lang::ALL {
            assert_eq!(Lang::parse(lang.code()), Some(lang));
            assert_eq!(Lang::parse(lang.endonym()), Some(lang));
            assert!(!lang.endonym().is_empty());
        }
    }

    #[test]
    fn every_phrase_is_written_in_every_language() {
        for lang in Lang::ALL {
            for (name, text) in lang.phrases() {
                assert!(
                    !text.trim().is_empty(),
                    "{}: '{name}' has nothing to say",
                    lang.code()
                );
            }
        }
    }

    /// A phrase left in English is the failure mode a half-finished
    /// localization has, and it is invisible on a screen nobody reads in
    /// that language. The exceptions are named, and each is a marker or
    /// a key name rather than a word.
    #[test]
    fn every_phrase_is_actually_translated() {
        // Markers and proper names, not words. `<DIR>` is the same four
        // letters in every language, exactly as `/`, `@`, and `@!` are;
        // and a file format's own name - `Markdown`, `PDF`, `TOML` - is
        // spelled the way its authors spell it, in any language.
        const SHARED: [&str; 13] = [
            "dir_marker",
            "filekind_markdown",
            "filekind_pdf",
            "filekind_rust",
            "filekind_toml",
            "filekind_json",
            "filekind_yaml",
            "filekind_html",
            "filekind_css",
            "filekind_javascript",
            "filekind_typescript",
            "filekind_python",
            "filekind_shell",
        ];
        let english = Lang::En.phrases();
        let chinese = Lang::ZhTw.phrases();
        assert_eq!(english.len(), chinese.len());
        for ((name, en), (_, zh)) in english.iter().zip(&chinese) {
            if SHARED.contains(name) {
                assert_eq!(en, zh, "'{name}' is meant to be the same in both");
                continue;
            }
            assert_ne!(
                en, zh,
                "'{name}' is still English in Traditional Chinese: {en:?}"
            );
        }
    }

    #[test]
    fn every_hint_row_is_written_in_every_language() {
        for lang in Lang::ALL {
            for (name, hints) in lang.hint_rows() {
                assert!(!hints.is_empty(), "{}: '{name}' has no hints", lang.code());
                for hint in hints {
                    assert!(
                        !hint.trim().is_empty(),
                        "{}: '{name}' has a blank hint",
                        lang.code()
                    );
                }
            }
        }
    }

    /// The rule that makes CJK safe: a phrase that lands in a
    /// fixed-width column is measured in *cells*, and a Han character
    /// owns two of them. `59分鐘前` is eight columns where `59m` is
    /// three, so the column has to be the one the language declares.
    #[test]
    fn every_age_fits_the_column_its_language_reserves() {
        const SECONDS: [u64; 14] = [
            0,
            1,
            59,
            60,
            61,
            3599,
            3600,
            86_399,
            86_400,
            604_799,
            604_800,
            31_535_999,
            31_536_000,
            31_536_000 * 99,
        ];
        for lang in Lang::ALL {
            for seconds in SECONDS {
                let age = lang.age(seconds);
                assert!(
                    display_width(&age) <= lang.age_width(),
                    "{}: '{age}' is {} columns, wider than the {} reserved",
                    lang.code(),
                    display_width(&age),
                    lang.age_width()
                );
                assert!(!age.is_empty());
            }
        }
    }

    #[test]
    fn traditional_chinese_says_ago_in_the_age_itself() {
        // The listing column and the spoken status row want the same
        // string in Chinese, because `2秒` alone is a duration and
        // `2秒前` is a moment in the past.
        assert_eq!(Lang::ZhTw.age(2), "2秒前");
        assert_eq!(Lang::ZhTw.age(5 * 60), "5分鐘前");
        assert_eq!(Lang::ZhTw.age(3 * 3600), "3小時前");
        assert_eq!(Lang::ZhTw.age(86_400), "1天前");
        assert_eq!(Lang::ZhTw.age_phrase("2秒前"), "2秒前");
        // English says it once, on the status row only.
        assert_eq!(Lang::En.age(2), "2s");
        assert_eq!(Lang::En.age_phrase("2s"), "2s ago");
    }

    #[test]
    fn every_preview_label_fits_the_column_its_language_reserves() {
        const FIELDS: [PreviewField; 7] = [
            PreviewField::Path,
            PreviewField::Symlink,
            PreviewField::Type,
            PreviewField::Size,
            PreviewField::Mode,
            PreviewField::Modified,
            PreviewField::Entries,
        ];
        for lang in Lang::ALL {
            for field in FIELDS {
                let label = lang.preview_label(field);
                assert!(
                    display_width(label) < lang.preview_label_width(),
                    "{}: '{label}' leaves no gap before its value",
                    lang.code()
                );
            }
        }
    }

    /// Every reason and usage line is written in both languages: they
    /// reach the message log, which is where a refused operation is
    /// explained.
    #[test]
    fn every_reason_and_usage_line_is_written_in_every_language() {
        const REASONS: [Reason; 12] = [
            Reason::TildeUser,
            Reason::EmptyPath,
            Reason::NoFileName,
            Reason::EmptyName,
            Reason::DotReserved,
            Reason::NameHasSlash,
            Reason::NameHasNul,
            Reason::RootNotTrashable,
            Reason::ParentNotAnEntry,
            Reason::NoParentDirectory,
            Reason::CurrentNotAnEntry,
            Reason::TrashMacOsOnly,
        ];
        for reason in REASONS {
            assert_ne!(
                Lang::En.reason(reason),
                Lang::ZhTw.reason(reason),
                "{reason:?} is still English in Traditional Chinese"
            );
            assert!(!Lang::ZhTw.reason(reason).trim().is_empty());
        }
        const USAGES: [Usage; 7] = [
            Usage::Cd,
            Usage::Move,
            Usage::Rename,
            Usage::Trash,
            Usage::Summarize,
            Usage::Log,
            Usage::Language,
        ];
        for usage in USAGES {
            assert_ne!(
                Lang::En.usage_line("cmd", usage),
                Lang::ZhTw.usage_line("cmd", usage),
                "{usage:?} is still English in Traditional Chinese"
            );
        }
        // `Usage::None` explains nothing in either language, so the two
        // differ only by the word in front of the command.
        assert!(Lang::ZhTw.usage_line("open", Usage::None).contains("open"));
    }

    #[test]
    fn the_help_screen_is_written_in_both_languages_and_documents_the_same_keys() {
        for lang in Lang::ALL {
            let help = lang.help_lines().join("\n");
            assert!(help.len() > 500, "{}: the help is a stub", lang.code());
            for key in [
                "j / k",
                "d / u",
                "f / b",
                "g / G",
                "n / N",
                "0-9",
                "  S ",
                "  L ",
                "Enter, c",
                "1 - 5",
                "Enter, m",
                "cd ",
                "rename ",
                "delete, trash",
                "summarize, summary",
                "log, job",
                "lang ",
                "help ",
                "quit ",
                "Ctrl-C",
            ] {
                assert!(
                    help.contains(key),
                    "{}: the help never mentions '{key}'",
                    lang.code()
                );
            }
        }
        // The Chinese help is written, not transliterated.
        let chinese = Lang::ZhTw.help_lines().join("\n");
        for word in ["說明", "垃圾桶", "閱讀模式", "目錄選擇器", "安全性"] {
            assert!(chinese.contains(word), "the help never says '{word}'");
        }
    }

    #[test]
    fn the_keys_row_placeholder_follows_the_character_set_in_force() {
        let unicode = keys_row(Lang::ZhTw.picker_keys(), "·");
        let ascii = keys_row(Lang::ZhTw.picker_keys(), "-");
        assert!(unicode.contains('·') && !unicode.contains("{dot}"));
        assert!(ascii.contains(" - ") && !ascii.contains('·'));
    }

    #[test]
    fn every_keys_row_fills_its_placeholder() {
        for lang in Lang::ALL {
            for template in [
                lang.picker_keys(),
                lang.selector_keys(),
                lang.provider_keys(),
                lang.reader_keys(),
            ] {
                assert!(
                    template.contains("{dot}"),
                    "{}: a keys row with no separator to fill: {template:?}",
                    lang.code()
                );
                assert!(!keys_row(template, "·").contains("{dot}"));
            }
        }
    }

    /// Every operation the message log can name is named in both
    /// languages, and no message hard-codes a prefix beside
    /// [`Lang::op_name`] where the two could drift apart.
    #[test]
    fn every_message_prefix_comes_from_one_table() {
        const OPS: [Op; 12] = [
            Op::Cd,
            Op::Move,
            Op::Rename,
            Op::Delete,
            Op::Open,
            Op::Edit,
            Op::Preview,
            Op::Read,
            Op::Log,
            Op::Summarize,
            Op::Language,
            Op::Agent,
        ];
        for op in OPS {
            assert!(!Lang::ZhTw.op_name(op).trim().is_empty(), "{op:?}");
            // `agent` is the seam's name, not a word, so it is the one
            // that is deliberately the same in both.
            if op != Op::Agent {
                assert_ne!(
                    Lang::En.op_name(op),
                    Lang::ZhTw.op_name(op),
                    "{op:?} is still English in Traditional Chinese"
                );
            }
            assert_eq!(
                Lang::ZhTw.op_says(op, "細節"),
                format!("{}: 細節", Lang::ZhTw.op_name(op))
            );
        }
        // Nothing in the fixed table opens with an English command name
        // followed by a colon: a message that names an operation goes
        // through `op_says`, so there is one place the prefix lives.
        for (name, text) in Lang::ZhTw.phrases() {
            for op in OPS {
                let english = format!("{}:", Lang::En.op_name(op));
                assert!(
                    !text.trim_start().starts_with(&english),
                    "'{name}' hard-codes the English prefix {english:?}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn a_message_named_after_an_operation_is_named_in_the_screens_language() {
        assert_eq!(
            Lang::En.move_same_place(),
            "move: source and destination are the same"
        );
        assert_eq!(Lang::ZhTw.move_same_place(), "移動: 來源與目標相同");
        assert_eq!(Lang::ZhTw.home_unknown(), "切換目錄: 無法判斷家目錄");
        assert_eq!(Lang::ZhTw.summarize_no_files(), "摘要: 尚未選取檔案");
        assert_eq!(Lang::ZhTw.language_saved("/x"), "語言: 已儲存至 /x");
        assert_eq!(
            Lang::ZhTw.opening_in_editor("a.txt", "nvim"),
            "編輯: 以 nvim 開啟 'a.txt'"
        );
    }

    /// A run that fails is where a half-finished localization shows,
    /// because it is the screen a user reads at the worst moment.
    #[test]
    fn every_failure_a_run_can_report_is_written_in_every_language() {
        let phrases: [fn(Lang) -> String; 8] = [
            |l| l.provider_wrote_nothing().to_string(),
            |l| l.provider_wrote_no_summary().to_string(),
            |l| l.run_stopped().to_string(),
            |l| l.run_without_result().to_string(),
            |l| l.could_not_reserve("/tmp/a.md", "denied"),
            |l| l.could_not_run("agy", "no such file"),
            |l| l.could_not_write("/tmp/a.md", "disk full"),
            |l| l.could_not_record("why", Some("/tmp/a.md"), "denied"),
        ];
        for (index, phrase) in phrases.iter().enumerate() {
            let (en, zh) = (phrase(Lang::En), phrase(Lang::ZhTw));
            assert!(!zh.trim().is_empty(), "failure {index} says nothing");
            assert_ne!(en, zh, "failure {index} is still English: {en:?}");
        }
        // The path and whatever the OS said travel through untouched:
        // they are evidence, not prose.
        let zh = Lang::ZhTw.could_not_run("agy", "No such file or directory");
        assert!(
            zh.contains("agy") && zh.contains("No such file or directory"),
            "{zh}"
        );
        assert_eq!(
            Lang::ZhTw.could_not_record("原因", None, "denied"),
            "原因；也無法記錄這次失敗：denied"
        );
    }

    #[test]
    fn a_timestamp_before_the_epoch_is_said_in_both_languages() {
        assert_eq!(Lang::En.before_the_epoch(), "before 1970");
        assert_eq!(Lang::ZhTw.before_the_epoch(), "1970 之前");
    }

    /// The strings the brief pins down, exactly as a reader of the
    /// screen would see them.
    #[test]
    fn the_traditional_chinese_screen_says_what_it_is_meant_to_say() {
        let zh = Lang::ZhTw;
        assert_eq!(zh.ladder_summary(2, 7, "·"), "階層 2 · 7 個項目");
        assert_eq!(zh.cwd_line("/tmp/x"), "目錄: /tmp/x");
        assert_eq!(zh.row_of(1, 9), "第 1 列，共 9 列");
        assert_eq!(zh.all_rows_shown(), "所有項目已顯示");
        assert_eq!(zh.kind_file(), "檔案");
        assert_eq!(zh.no_matching_entries(), "(無相符項目)");
        assert_eq!(
            zh.confirm_line(&zh.describe_move("a", "b")),
            "確認移動 'a' -> 'b' (y/n)"
        );
        assert_eq!(
            zh.confirm_line(&zh.describe_trash("a.txt")),
            "確認將 'a.txt' 移至垃圾桶 (y/n)"
        );
        assert_eq!(
            zh.quit_question(),
            "背景任務執行中：確認終止 AI 摘要並離開？(y/n)"
        );
        assert_eq!(zh.destination_line("/tmp"), "目標: /tmp");
        assert_eq!(zh.provider_title().trim(), "選擇 AI 模型");
        assert_eq!(zh.provider_default_mark().trim(), "[預設]");
        assert_eq!(zh.job_status(3, "agy"), "[AI: 正在使用 agy 摘要 3 個檔案]");
        assert_eq!(
            zh.reader_position(4, 90, 12, "·"),
            "第 4 行，共 90 行 · 12%"
        );
        assert_eq!(zh.activity_waiting(), "等待輸出...");
        assert_eq!(zh.activity_thinking(), "分析中...");
        assert_eq!(zh.activity_ended(), "完成");
        assert_eq!(
            keys_row(zh.picker_keys(), "·").trim(),
            "j/k 瀏覽 · l 進入 · h 上層 · Enter/m 選取 · q 取消"
        );
        assert_eq!(
            keys_row(zh.provider_keys(), "·").trim(),
            "1-5 選擇 · Enter 使用預設 (ag) · q 取消"
        );
        assert_eq!(
            zh.hints_browse()[..9].join(" · "),
            "j/k 移動 · l/Enter 進入 · h 上層 · 0-9 跳轉 · / 搜尋 · : 指令 · S AI摘要 · ? 說明 · q 離開"
        );
    }
}
