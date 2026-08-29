//! The provider session a summary run belongs to: how it is recognized
//! in the provider's own output, and how it is written down.
//!
//! An AI CLI names the conversation it just opened somewhere in its
//! banner - `codex exec` prints `session id: <uuid>` on stderr before it
//! says anything else. Recognizing that is what lets Filecraft point at
//! the run afterwards: the log header shows it while the job is alive,
//! and the finished summary carries a footer naming the command that
//! reopens it in the provider's own CLI.
//!
//! Everything here is a pure function of one line of text. Nothing in
//! this module runs, spawns, or resumes anything - it only reads and
//! writes words.

/// Shortest run of characters that can be an identifier. Below this a
/// match is far more likely to be prose ("session: done") than an id.
const MIN_ID: usize = 6;

/// Longest identifier accepted. A session id is a UUID or a short token;
/// anything longer is a sentence that happened to follow the key, and it
/// must never reach a header row or a Markdown footer.
const MAX_ID: usize = 128;

/// The keys a provider may announce its session under, lowercased. Each
/// is matched literally against a lowercased copy of the line, so a
/// banner row (`session id: <uuid>`) and a JSON envelope
/// (`"session_id":"<uuid>"`) are both found by the same pass.
const KEYS: [&str; 10] = [
    "session_id",
    "session id",
    "sessionid",
    "conversation_id",
    "conversation id",
    "conversationid",
    "thread_id",
    "thread id",
    "run_id",
    "run id",
];

/// The prefix of a self-describing identifier - the shape that carries
/// its own name (`session_014A3EnRnr...`) and needs no key in front.
const BARE_PREFIX: &str = "session_";

/// Whether `c` may appear inside an identifier.
///
/// Deliberately narrow: an id read out of a provider's output is drawn
/// on a header row and written into a Markdown file, so it is restricted
/// to the characters real session ids use and nothing else. Anything
/// with a space, a quote, or a control character in it is not an id.
fn id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Whether `id` is plausible enough to show and to write down.
///
/// Length-bounded, restricted to [`id_char`], and required to carry at
/// least one digit: every session id these CLIs mint is a UUID or a
/// random token, and the requirement is what keeps a word like
/// `session id: unknown` from being reported as one.
pub fn is_id(id: &str) -> bool {
    let count = id.chars().count();
    (MIN_ID..=MAX_ID).contains(&count)
        && id.chars().all(id_char)
        && id.chars().any(|c| c.is_ascii_digit())
}

/// The identifier starting at `from` in `line`, if one does.
fn id_at(line: &str, from: usize) -> Option<String> {
    let id: String = line[from..].chars().take_while(|c| id_char(*c)).collect();
    is_id(&id).then_some(id)
}

/// The session identifier announced on one line of provider output, if
/// it announces one.
///
/// Two shapes are recognized, in this order:
///
/// 1. A key and a value - `session id: <uuid>`, `session_id=<uuid>`,
///    `"conversation_id": "<uuid>"`. The separator may be `:` or `=`,
///    and quotes and spaces around it are skipped.
/// 2. A bare `session_<token>`, which names itself.
///
/// The first plausible identifier on the line wins. A line that carries
/// a key with no value after it (`session id:` alone, or `session id:
/// none`) yields nothing rather than a placeholder.
pub fn scan(line: &str) -> Option<String> {
    // `to_ascii_lowercase` is byte-for-byte the same length, so an index
    // found in the lowered copy is the same index in the original - which
    // is what keeps an uppercase id from being lowercased into a
    // different id.
    let lowered = line.to_ascii_lowercase();
    let mut best: Option<(usize, String)> = None;

    for key in KEYS {
        let mut from = 0;
        while let Some(hit) = lowered[from..].find(key) {
            let after = from + hit + key.len();
            from = after;
            // The key must be a word of its own: `my_session_id` is one,
            // `xsession_id` is not.
            let before = lowered[..from - key.len()].chars().next_back();
            if before.is_some_and(id_char) {
                continue;
            }
            let Some(start) = value_start(&lowered, after) else {
                continue;
            };
            if let Some(id) = id_at(line, start) {
                if best.as_ref().is_none_or(|(at, _)| start < *at) {
                    best = Some((start, id));
                }
                break;
            }
        }
    }

    if let Some((_, id)) = best {
        return Some(id);
    }

    // A bare `session_<token>`: it names itself, so no key is needed.
    let mut from = 0;
    while let Some(hit) = lowered[from..].find(BARE_PREFIX) {
        let start = from + hit;
        from = start + BARE_PREFIX.len();
        let before = lowered[..start].chars().next_back();
        if before.is_some_and(id_char) {
            continue;
        }
        if let Some(id) = id_at(line, start) {
            return Some(id);
        }
    }
    None
}

/// Where the value after a key starts: past the separator, any quotes,
/// and any whitespace.
///
/// `None` when the value runs straight into the key (`session_idabc`),
/// so a key that is only part of a longer word never yields the rest of
/// it, and `None` again when a second separator follows the first, which
/// is a key with no value rather than a value.
fn value_start(lowered: &str, after: usize) -> Option<usize> {
    let mut index = after;
    let mut separated = false;
    let mut skipped = false;
    for c in lowered[after..].chars() {
        match c {
            ':' | '=' => {
                if separated {
                    return None;
                }
                separated = true;
            }
            '"' | '\'' | ' ' | '\t' => skipped = true,
            _ => break,
        }
        index += c.len_utf8();
    }
    (separated || skipped).then_some(index)
}

/// The footer a finished summary carries, naming the run that wrote it.
///
/// One Markdown blockquote, so it reads as an aside wherever the summary
/// is opened and never looks like part of what the documents said.
/// `resume` is the provider's own reopen command - see
/// [`Provider::resume_command`](crate::summarize::Provider::resume_command) -
/// and a run whose provider never announced a session says so instead of
/// printing a command that would not work.
pub fn footer(program: &str, id: Option<&str>, resume: Option<&str>) -> String {
    match (id, resume) {
        (Some(id), Some(resume)) => {
            format!("> Provider: {program} | Session: {id} | Resume with: {resume}")
        }
        (Some(id), None) => format!("> Provider: {program} | Session: {id}"),
        _ => format!("> Provider: {program} | Session: not reported"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line `codex exec` really prints, taken from a live run. It
    /// arrives on stderr, in a banner, before anything else - which is
    /// why the log captures both streams.
    #[test]
    fn the_codex_banner_line_is_recognized() {
        assert_eq!(
            scan("session id: 01a04eef-d4a6-7232-831f-e8faf5c42241"),
            Some("01a04eef-d4a6-7232-831f-e8faf5c42241".to_string())
        );
    }

    #[test]
    fn every_key_spelling_and_separator_is_read() {
        for line in [
            "session_id: abc-123456",
            "session id = abc-123456",
            "Session ID: abc-123456",
            "SESSIONID:abc-123456",
            r#"{"session_id":"abc-123456"}"#,
            r#"  "conversation_id": "abc-123456","#,
            "conversation id: abc-123456",
            "thread_id=abc-123456",
            "run id: abc-123456",
            "[info] resumed session_id 'abc-123456' ok",
        ] {
            assert_eq!(scan(line), Some("abc-123456".to_string()), "{line:?}");
        }
    }

    #[test]
    fn a_self_naming_token_needs_no_key() {
        assert_eq!(
            scan("see https://claude.ai/code/session_014A3EnRnrfLJyywsSV2jxiX"),
            Some("session_014A3EnRnrfLJyywsSV2jxiX".to_string())
        );
    }

    #[test]
    fn prose_and_placeholders_are_not_identifiers() {
        for line in [
            "starting a new session",
            "session id:",
            "session id: none",
            "session id: <id>",
            "conversation resumed",
            "session_id: abc",
            "the session was interrupted",
            "my_session_id: shorter",
            "",
        ] {
            assert_eq!(scan(line), None, "{line:?}");
        }
    }

    /// A key spelled inside a longer word is not that key: `xsession_id`
    /// belongs to whatever is printing it, not to us.
    #[test]
    fn a_key_must_be_a_word_of_its_own() {
        assert_eq!(scan("xsession_id: 0123456789"), None);
        assert_eq!(scan("-session_id: 0123456789"), None);
        assert_eq!(
            scan("[codex] session_id: 0123456789"),
            Some("0123456789".to_string())
        );
    }

    /// The value keeps its own case: an id is copied out of the line, not
    /// out of the lowercased copy used to find it.
    #[test]
    fn an_identifier_keeps_its_case() {
        assert_eq!(
            scan("Session ID: AbCdEf-123456"),
            Some("AbCdEf-123456".to_string())
        );
    }

    /// A line naming two of them reports the first, so a log that echoes
    /// a previous run's id after its own never renames the run.
    #[test]
    fn the_first_identifier_on_a_line_wins() {
        assert_eq!(
            scan("session_id: 111111-aaa conversation_id: 222222-bbb"),
            Some("111111-aaa".to_string())
        );
        assert_eq!(
            scan("conversation_id: 222222-bbb session_id: 111111-aaa"),
            Some("222222-bbb".to_string())
        );
    }

    /// Nothing that could carry an escape sequence, a quote, or a path
    /// into a header row or a Markdown file is an identifier.
    #[test]
    fn an_identifier_is_bounded_and_plain() {
        assert!(is_id("01a04eef-d4a6-7232-831f-e8faf5c42241"));
        assert!(is_id("session_014A3E"));
        assert!(!is_id("abc12"), "too short");
        assert!(!is_id(&"a1".repeat(65)), "too long");
        assert!(!is_id("all-letters-here"), "no digits: prose");
        assert!(!is_id("has space1"));
        assert!(!is_id("/etc/passwd1"));
        assert!(!is_id("id\u{1b}[31m1"));
    }

    #[test]
    fn the_footer_names_the_run_and_how_to_reopen_it() {
        assert_eq!(
            footer(
                "agy",
                Some("abc-123456"),
                Some("agy --conversation abc-123456")
            ),
            "> Provider: agy | Session: abc-123456 | \
             Resume with: agy --conversation abc-123456"
        );
        assert_eq!(
            footer("codex", Some("u1"), None),
            "> Provider: codex | Session: u1"
        );
        assert_eq!(
            footer("kimi", None, None),
            "> Provider: kimi | Session: not reported"
        );
    }
}
