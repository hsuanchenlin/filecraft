//! Integration tests for `l` / Right on a file the built-in reader
//! cannot draw.
//!
//! These drive the same public API `main.rs` drives - abstract key
//! inputs in, [`Effect`]s out - against real files on disk, so the whole
//! path a user takes is covered: the listing is read from the
//! filesystem, the entry is selected by name, the key is pressed, and
//! the effect the event loop would act on is asserted. Only the final
//! `posix_spawn` is left out, because launching Preview is not something
//! a test suite may do to the machine running it.

use std::fs;

use filecraft::app::{App, Effect, KeyInput, Level, Mode};
use filecraft::i18n::Lang;
use filecraft::nav::NavState;

/// The first bytes of a real PDF: a version header and the binary
/// comment line every producer writes so transfer programs treat the
/// file as binary. Nothing here is a NUL, which is exactly why the
/// decision is made on the name.
const PDF_HEAD: &[u8] = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n1 0 obj\n<< /Type /Catalog >>\nendobj\n";

fn app_at(dir: &std::path::Path, lang: Lang) -> App {
    let nav = NavState::new(dir).expect("the fixture directory must be readable");
    App::new(nav, None, false, None, lang)
}

/// Put the cursor on `name`, the way arrowing down to it would.
fn select(app: &mut App, name: &str) {
    let row = app
        .nav
        .visible()
        .iter()
        .position(|&i| app.nav.entries[i].name == name)
        .unwrap_or_else(|| panic!("'{name}' is not in the listing"));
    app.nav.cursor = row;
}

fn last_text(app: &App) -> String {
    app.messages
        .last()
        .expect("the message log must say what happened")
        .text
        .clone()
}

#[test]
fn l_and_right_hand_a_real_pdf_to_the_macos_default_application() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("report.pdf"), PDF_HEAD).unwrap();

    for key in [KeyInput::Char('l'), KeyInput::Right] {
        let mut app = app_at(tmp.path(), Lang::En);
        select(&mut app, "report.pdf");
        let effect = app.handle_key(key);

        if cfg!(target_os = "macos") {
            let Effect::SpawnDetached { argv } = effect else {
                panic!("{key:?} on a PDF must spawn macOS open, got {effect:?}");
            };
            assert_eq!(argv[0], "/usr/bin/open");
            assert_eq!(argv[1], "--");
            // The listing works from the canonicalized directory, so
            // the path handed over is the resolved one - what `open`
            // needs, and never a `..` walked back through a symlink.
            let expected = tmp.path().canonicalize().unwrap().join("report.pdf");
            assert_eq!(argv[2], expected.display().to_string());
            assert_eq!(app.messages.last().unwrap().level, Level::Ok);
            assert_eq!(
                last_text(&app),
                "open: handing 'report.pdf' to the macOS default application"
            );
        } else {
            assert_eq!(effect, Effect::None);
            assert_eq!(app.messages.last().unwrap().level, Level::Error);
            assert!(last_text(&app).contains("macOS"), "{}", last_text(&app));
        }

        // Either way the listing is still what is on screen: the reader
        // never opened and the terminal was never handed away.
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.nav.cwd, tmp.path().canonicalize().unwrap());
    }
}

#[test]
fn the_pdf_is_never_touched_by_the_key_that_opens_it() {
    let tmp = tempfile::tempdir().unwrap();
    let pdf = tmp.path().join("report.pdf");
    fs::write(&pdf, PDF_HEAD).unwrap();
    let before = fs::metadata(&pdf).unwrap();

    let mut app = app_at(tmp.path(), Lang::En);
    select(&mut app, "report.pdf");
    app.handle_key(KeyInput::Char('l'));

    let after = fs::metadata(&pdf).unwrap();
    assert_eq!(fs::read(&pdf).unwrap(), PDF_HEAD);
    assert_eq!(before.len(), after.len());
    assert_eq!(before.permissions(), after.permissions());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
}

#[test]
fn l_and_right_still_read_text_and_still_enter_directories() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("sub")).unwrap();
    fs::write(tmp.path().join("notes.md"), "# Title\n\nbody\n").unwrap();
    fs::write(tmp.path().join("plain.txt"), "just words\n").unwrap();
    fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();

    for key in [KeyInput::Char('l'), KeyInput::Right] {
        for name in ["notes.md", "plain.txt", "main.rs"] {
            let mut app = app_at(tmp.path(), Lang::En);
            select(&mut app, name);
            assert_eq!(app.handle_key(key), Effect::None, "{name}");
            assert!(
                matches!(app.mode, Mode::Pager(_)),
                "{key:?} on '{name}' must open the built-in reader, got {:?}",
                app.mode
            );
        }

        let mut app = app_at(tmp.path(), Lang::En);
        select(&mut app, "sub");
        assert_eq!(app.handle_key(key), Effect::None);
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.nav.cwd.ends_with("sub"), "{:?}", app.nav.cwd);
    }
}

#[test]
fn the_message_is_written_in_the_screen_language() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("report.pdf"), PDF_HEAD).unwrap();

    let mut english = app_at(tmp.path(), Lang::En);
    select(&mut english, "report.pdf");
    english.handle_key(KeyInput::Char('l'));

    let mut chinese = app_at(tmp.path(), Lang::ZhTw);
    select(&mut chinese, "report.pdf");
    chinese.handle_key(KeyInput::Char('l'));

    if cfg!(target_os = "macos") {
        assert_eq!(
            last_text(&english),
            "open: handing 'report.pdf' to the macOS default application"
        );
        assert_eq!(last_text(&chinese), "開啟: 正將 'report.pdf' 交給預設程式");
    } else {
        assert!(last_text(&english).starts_with("open: "));
        assert!(last_text(&chinese).starts_with("開啟: "));
    }
}
