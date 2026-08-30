//! Who owns a file, as a name rather than a number.
//!
//! The `owner` column wants `hsuan:staff`, not `501:20`, and the system
//! user database is the only thing that knows the difference. There is
//! no std API for it: on macOS the answer lives in Directory Services
//! rather than in `/etc/passwd`, so reading that file would be right on
//! Linux and wrong here. `getpwuid_r`/`getgrgid_r` ask the same resolver
//! `ls -l` asks, whatever is behind it.
//!
//! Two properties matter to the listing:
//!
//! - **A miss is a number, never a blank.** A uid with no entry - a file
//!   copied off another machine, a deleted account - still identifies
//!   its owner, so the column is never empty and never a lie.
//! - **One lookup per distinct id, not per row.** A directory of ten
//!   thousand files owned by one person is one query. The memo is
//!   process-wide and never invalidated, because a uid's name does not
//!   change under a running program in any way the listing should chase.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// The login name for `uid`, or the number when the system has no entry
/// for it.
pub fn user(uid: u32) -> String {
    memo(users(), uid, user_name).unwrap_or_else(|| uid.to_string())
}

/// The group name for `gid`, or the number when the system has no entry.
pub fn group(gid: u32) -> String {
    memo(groups(), gid, group_name).unwrap_or_else(|| gid.to_string())
}

fn users() -> &'static Mutex<HashMap<u32, Option<String>>> {
    static USERS: OnceLock<Mutex<HashMap<u32, Option<String>>>> = OnceLock::new();
    USERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn groups() -> &'static Mutex<HashMap<u32, Option<String>>> {
    static GROUPS: OnceLock<Mutex<HashMap<u32, Option<String>>>> = OnceLock::new();
    GROUPS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `lookup(id)`, remembered - including a miss, so an unknown uid is not
/// re-queried once per row for the rest of the session.
///
/// A poisoned memo is not worth failing a listing over: the lookup is
/// idempotent, so it is simply performed again.
fn memo(
    cache: &Mutex<HashMap<u32, Option<String>>>,
    id: u32,
    lookup: fn(u32) -> Option<String>,
) -> Option<String> {
    if let Ok(map) = cache.lock() {
        if let Some(hit) = map.get(&id) {
            return hit.clone();
        }
    }
    let found = lookup(id);
    if let Ok(mut map) = cache.lock() {
        map.insert(id, found.clone());
    }
    found
}

/// Largest buffer a name lookup is given before it is called hopeless.
/// `getpwuid_r` asks for more room with `ERANGE`; a resolver that wants
/// more than this is not answering a question about a file listing.
const MAX_BUFFER: usize = 64 * 1024;

/// Where the growth starts. `sysconf` has an opinion, and it is asked
/// for first; this is the floor when it has none.
const MIN_BUFFER: usize = 1024;

#[cfg(unix)]
fn suggested_buffer(name: libc::c_int) -> usize {
    // SAFETY: `sysconf` reads a system constant and touches no memory of
    // ours. A negative answer means "no opinion", which is not an error.
    let size = unsafe { libc::sysconf(name) };
    if size <= 0 {
        MIN_BUFFER
    } else {
        (size as usize).clamp(MIN_BUFFER, MAX_BUFFER)
    }
}

/// The login name for `uid`, straight from the system database.
#[cfg(unix)]
fn user_name(uid: u32) -> Option<String> {
    let mut capacity = suggested_buffer(libc::_SC_GETPW_R_SIZE_MAX);
    loop {
        let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let mut buffer = vec![0u8; capacity];
        // SAFETY: `passwd` and `found` are owned locals, and `buffer` is
        // a live allocation of exactly `capacity` bytes. `getpwuid_r`
        // writes only into those three, and the `pw_name` it points at
        // is inside `buffer`, which outlives the copy made from it.
        let code = unsafe {
            libc::getpwuid_r(
                uid as libc::uid_t,
                &mut passwd,
                buffer.as_mut_ptr() as *mut libc::c_char,
                capacity,
                &mut found,
            )
        };
        match code {
            0 if !found.is_null() => return copy_c_string(passwd.pw_name),
            // A zero return with a null result is "no such user", which
            // is an answer rather than a failure.
            0 => return None,
            libc::ERANGE if capacity < MAX_BUFFER => {
                capacity = (capacity * 2).min(MAX_BUFFER);
            }
            _ => return None,
        }
    }
}

/// The group name for `gid`, straight from the system database.
#[cfg(unix)]
fn group_name(gid: u32) -> Option<String> {
    let mut capacity = suggested_buffer(libc::_SC_GETGR_R_SIZE_MAX);
    loop {
        let mut record: libc::group = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::group = std::ptr::null_mut();
        let mut buffer = vec![0u8; capacity];
        // SAFETY: identical to `user_name` - owned locals plus a live
        // buffer of the length being reported, and nothing borrowed past
        // the string copy below.
        let code = unsafe {
            libc::getgrgid_r(
                gid as libc::gid_t,
                &mut record,
                buffer.as_mut_ptr() as *mut libc::c_char,
                capacity,
                &mut found,
            )
        };
        match code {
            0 if !found.is_null() => return copy_c_string(record.gr_name),
            0 => return None,
            libc::ERANGE if capacity < MAX_BUFFER => {
                capacity = (capacity * 2).min(MAX_BUFFER);
            }
            _ => return None,
        }
    }
}

/// A NUL-terminated name from the system database, as an owned `String`.
/// Lossy on purpose: a name is drawn on a terminal, and a byte that is
/// not UTF-8 must become a replacement character rather than lose the
/// row it is on.
#[cfg(unix)]
fn copy_c_string(raw: *const libc::c_char) -> Option<String> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` is non-null and points into the buffer the lookup
    // filled, which is still alive at the call site.
    let bytes = unsafe { std::ffi::CStr::from_ptr(raw) }.to_bytes();
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Off unix there is no user database to ask, so every id is its own
/// name and the column still says something true.
#[cfg(not(unix))]
fn user_name(_uid: u32) -> Option<String> {
    None
}

#[cfg(not(unix))]
fn group_name(_gid: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_running_users_own_id_resolves_to_a_name() {
        // SAFETY: `getuid` reads a process attribute and cannot fail.
        let uid = unsafe { libc::getuid() } as u32;
        let name = user(uid);
        assert!(!name.is_empty());
        // A real account has a name, not the number back again. On a
        // machine where it somehow does not, the number is still the
        // right answer, so only the shape is asserted.
        assert!(!name.contains(char::is_whitespace), "{name:?}");
    }

    #[cfg(unix)]
    #[test]
    fn root_is_named_root_and_its_group_resolves() {
        assert_eq!(user(0), "root");
        assert!(!group(0).is_empty());
    }

    #[test]
    fn an_id_the_system_does_not_know_reads_back_as_its_number() {
        // Far above any real account, and reserved by no platform.
        let orphan = 4_000_000_123u32;
        assert_eq!(user(orphan), orphan.to_string());
        assert_eq!(group(orphan), orphan.to_string());
    }

    #[test]
    fn a_repeated_lookup_is_answered_from_the_memo() {
        let orphan = 4_000_000_124u32;
        let first = user(orphan);
        // The second call must not disagree with the first, whichever
        // half of `memo` answered it.
        assert_eq!(first, user(orphan));
        assert!(users().lock().unwrap().contains_key(&orphan));
    }
}
