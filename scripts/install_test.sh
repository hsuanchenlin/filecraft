#!/usr/bin/env bash
#
# Unit tests for install.sh's PATH detection and profile editing.
#
# install.sh is sourced with FILECRAFT_INSTALL_LIB=1, which defines its
# functions and installs nothing, so every case below is a pure call.
# `cargo test` runs this file through tests/cli.rs, so CI covers it too.
#
# Usage: scripts/install_test.sh

set -uo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"

# A home directory the assertions can name, so a real $HOME cannot make a
# case pass or fail by accident.
REAL_HOME="$HOME"
HOME=/home/tester
export HOME

FILECRAFT_INSTALL_LIB=1 . "$ROOT/install.sh"

PASS=0
FAIL=0

ok() { PASS=$((PASS + 1)); }
bad() {
    FAIL=$((FAIL + 1))
    printf 'FAIL: %s\n' "$1" >&2
}

check_eq() {
    local want="$1" got="$2" what="$3"
    if [ "$want" = "$got" ]; then ok; else bad "$what: want [$want], got [$got]"; fi
}

check_true() {
    if "${@:2}"; then ok; else bad "$1: expected success"; fi
}

check_false() {
    if "${@:2}"; then bad "$1: expected failure"; else ok; fi
}

file_mode() {
    if stat -c '%a' "$1" >/dev/null 2>&1; then
        stat -c '%a' "$1"
    else
        stat -f '%Lp' "$1"
    fi
}

# ------------------------------------------------------- normalize_dir

check_eq "/home/tester/.cargo/bin" "$(normalize_dir '~/.cargo/bin')" "tilde"
check_eq "/home/tester/.cargo/bin" "$(normalize_dir '$HOME/.cargo/bin')" "\$HOME"
check_eq "/home/tester/.cargo/bin" "$(normalize_dir '${HOME}/.cargo/bin')" "\${HOME}"
check_eq "/home/tester" "$(normalize_dir '~')" "bare tilde"
check_eq "/usr/bin" "$(normalize_dir '/usr/bin/')" "trailing slash"
check_eq "/usr/bin" "$(normalize_dir '/usr/bin///')" "many trailing slashes"
check_eq "/usr/bin" "$(normalize_dir '/usr//bin')" "doubled separator"
check_eq "/usr/bin" "$(normalize_dir '/usr/./bin')" "dot component"
check_eq "/" "$(normalize_dir '/')" "root survives"
check_eq "/opt/tilde~dir" "$(normalize_dir '/opt/tilde~dir')" "tilde only expands at the front"
check_eq "usr/bin" "$(normalize_dir 'usr//bin/')" "relative paths stay relative"
check_eq "." "$(normalize_dir '')" "an empty directory is the current one"

# Splitting a path on IFS also globs unless globbing is off, which would
# quietly compare whatever is on disk instead of the entry itself.
check_eq "/opt/*" "$(normalize_dir '/opt/*')" "a star is a name, not a glob"
check_eq "/*" "$(normalize_dir '/*')" "a star at the root is a name too"
check_true "a starred PATH entry is compared literally" \
    path_contains_dir '/opt/*' '/usr/bin:/opt/*'
check_false "and matches nothing it is not" \
    path_contains_dir '/opt/anything' '/usr/bin:/opt/*'

# The functions must leave the caller's shell as they found it.
set +f
path_contains_dir '/x' '/y' || true
case "$-" in *f*) bad "path_contains_dir left globbing off" ;; *) ok ;; esac
set -f
path_contains_dir '/x' '/y' || true
case "$-" in *f*) ok ;; *) bad "path_contains_dir turned globbing back on" ;; esac
set +f

# ---------------------------------------------------- path_contains_dir

check_true "exact entry" \
    path_contains_dir '/home/tester/.cargo/bin' '/usr/bin:/home/tester/.cargo/bin:/bin'

# The reported bug: installed, but the shell is never told to look there.
check_false "the missing cargo bin directory" \
    path_contains_dir '/home/tester/.cargo/bin' '/opt/homebrew/bin:/usr/bin:/bin'

for spelling in '~/.cargo/bin' '$HOME/.cargo/bin' '${HOME}/.cargo/bin' \
                '/home/tester/.cargo/bin/' '/home/tester/./.cargo/bin'; do
    check_true "spelling $spelling" \
        path_contains_dir '/home/tester/.cargo/bin' "/usr/bin:$spelling"
done

check_false "a parent directory is not the directory" \
    path_contains_dir '/home/tester/.cargo/bin' '/home/tester/.cargo:/usr/bin'
check_false "a longer name is not the directory" \
    path_contains_dir '/home/tester/.cargo/bin' '/home/tester/.cargo/bin2'
check_false "an empty PATH reaches nothing" \
    path_contains_dir '/home/tester/.cargo/bin' ''
check_true "empty entries do not hide their neighbours" \
    path_contains_dir '/bin' ':/usr/bin::/bin:'

# --------------------------------------------- shell_kind / profile_for

check_eq "zsh" "$(shell_kind /bin/zsh)" "zsh"
check_eq "zsh" "$(shell_kind -zsh)" "login zsh"
check_eq "bash" "$(shell_kind /bin/bash)" "bash"
check_eq "bash" "$(shell_kind /bin/sh)" "sh"
check_eq "fish" "$(shell_kind /opt/homebrew/bin/fish)" "fish"
check_eq "other" "$(shell_kind /usr/bin/nu)" "unknown shell"
check_eq "other" "$(shell_kind '')" "unset SHELL"

check_eq "/home/tester/.zshrc" "$(profile_for zsh)" "zsh profile"
check_eq "/home/tester/.bashrc" "$(profile_for bash)" "bash profile"
check_eq "/home/tester/.config/fish/config.fish" "$(profile_for fish)" "fish profile"
check_eq "/home/tester/.profile" "$(profile_for other)" "fallback profile"

# ------------------------------------------ portable_dir / export_line_for

check_eq '$HOME/.cargo/bin' "$(portable_dir /home/tester/.cargo/bin)" "under home"
check_eq '/opt/filecraft/bin' "$(portable_dir /opt/filecraft/bin)" "outside home"

check_eq 'export PATH="$HOME/.cargo/bin:$PATH"' \
    "$(export_line_for zsh /home/tester/.cargo/bin)" "zsh export line"
check_eq 'export PATH="$HOME/.cargo/bin:$PATH"' \
    "$(export_line_for bash '~/.cargo/bin')" "bash export line"
check_eq 'fish_add_path $HOME/.cargo/bin' \
    "$(export_line_for fish /home/tester/.cargo/bin)" "fish line"

# ------------------------------------- profile_has_dir / add_to_profile

TMP="$(mktemp -d)"
trap 'rm -rf -- "$TMP"' EXIT

check_false "a file that does not exist adds nothing" \
    profile_has_dir "$TMP/absent" /home/tester/.cargo/bin

printf 'alias ll="ls -l"\n' > "$TMP/plain"
check_false "an unrelated profile" profile_has_dir "$TMP/plain" /home/tester/.cargo/bin

for existing in 'export PATH="$HOME/.cargo/bin:$PATH"' \
                'export PATH=~/.cargo/bin:$PATH' \
                'export PATH="/home/tester/.cargo/bin:$PATH"' \
                '. "$HOME/.cargo/env"'; do
    printf '%s\n' "$existing" > "$TMP/existing"
    check_true "already configured by [$existing]" \
        profile_has_dir "$TMP/existing" /home/tester/.cargo/bin
done

printf '. "$HOME/.cargo/env"\n' > "$TMP/cargo-env"
check_true "cargo env configures the default Cargo bin" \
    profile_has_dir "$TMP/cargo-env" /home/tester/.cargo/bin
check_false "cargo env does not configure a custom install root" \
    profile_has_dir "$TMP/cargo-env" /custom/root/bin

# A commented-out line is not configuration.
printf '# export PATH="$HOME/.cargo/bin:$PATH"\n' > "$TMP/commented"
check_false "a commented line does not count" \
    profile_has_dir "$TMP/commented" /home/tester/.cargo/bin
printf 'export OTHER=value # export PATH="$HOME/.cargo/bin:$PATH"\n' > "$TMP/inline-comment"
check_false "a PATH fragment in an inline comment does not count" \
    profile_has_dir "$TMP/inline-comment" /home/tester/.cargo/bin
printf 'export PATH="$HOME/.cargo/bin:$PATH" # Cargo binaries\n' > "$TMP/trailing-comment"
check_true "a real export with a trailing comment counts" \
    profile_has_dir "$TMP/trailing-comment" /home/tester/.cargo/bin

# Appending is idempotent: the second run finds its own marker and stops.
DRY_RUN=no
: > "$TMP/zshrc"
check_true "first append" add_to_profile "$TMP/zshrc" zsh /home/tester/.cargo/bin
check_false "second append is refused" \
    add_to_profile "$TMP/zshrc" zsh /home/tester/.cargo/bin
check_eq "1" "$(grep -c 'export PATH=' "$TMP/zshrc")" "exactly one PATH line"
check_true "the appended block is what we would have printed" \
    grep -qF 'export PATH="$HOME/.cargo/bin:$PATH"' "$TMP/zshrc"
check_true "the block is marked so it can be found again" \
    grep -qF '# >>> filecraft install >>>' "$TMP/zshrc"
check_true "and the appended block reads as configured" \
    profile_has_dir "$TMP/zshrc" /home/tester/.cargo/bin
check_true "a changed install root replaces the block" \
    add_to_profile "$TMP/zshrc" zsh /custom/root/bin
check_true "the replacement contains the new PATH line" \
    grep -qF 'export PATH="/custom/root/bin:$PATH"' "$TMP/zshrc"
check_false "the replacement removes the old PATH line" \
    grep -qF 'export PATH="$HOME/.cargo/bin:$PATH"' "$TMP/zshrc"
check_eq "1" "$(grep -c 'export PATH=' "$TMP/zshrc")" "one replaced PATH line"
check_eq "1" "$(grep -cF "$BEGIN_MARKER" "$TMP/zshrc")" "one begin marker"
check_eq "1" "$(grep -cF "$END_MARKER" "$TMP/zshrc")" "one end marker"
check_false "the replaced block remains idempotent" \
    add_to_profile "$TMP/zshrc" zsh /custom/root/bin
check_eq "1" "$(grep -c 'export PATH=' "$TMP/zshrc")" "one PATH line after rerun"
check_eq "0" "$(find "$TMP" -maxdepth 1 -name 'zshrc.filecraft.*' | wc -l | tr -d ' ')" \
    "successful replacement leaves no temporary file"

for malformed in missing-end end-first duplicate; do
    case "$malformed" in
        missing-end) printf 'before\n%s\nafter\n' "$BEGIN_MARKER" > "$TMP/$malformed" ;;
        end-first) printf 'before\n%s\n%s\nafter\n' "$END_MARKER" "$BEGIN_MARKER" > "$TMP/$malformed" ;;
        duplicate) printf '%s\nold\n%s\n%s\nold\n%s\n' \
            "$BEGIN_MARKER" "$END_MARKER" "$BEGIN_MARKER" "$END_MARKER" > "$TMP/$malformed" ;;
    esac
    cp "$TMP/$malformed" "$TMP/$malformed.before"
    check_false "$malformed markers are refused" \
        add_to_profile "$TMP/$malformed" zsh /custom/root/bin
    check_true "$malformed markers leave the profile byte-identical" \
        cmp -s "$TMP/$malformed.before" "$TMP/$malformed"
    check_eq "0" "$(find "$TMP" -maxdepth 1 -name "$malformed.filecraft.*" | wc -l | tr -d ' ')" \
        "$malformed failure leaves no temporary file"
done

cp "$TMP/zshrc" "$TMP/profile-target"
ln -s profile-target "$TMP/profile-link"
check_true "a symlinked profile is reconciled" \
    add_to_profile "$TMP/profile-link" zsh /symlink/root/bin
check_true "the profile path remains a symlink" test -L "$TMP/profile-link"
check_eq "profile-target" "$(readlink "$TMP/profile-link")" "the profile symlink target is unchanged"
check_true "the symlink target receives the new line" \
    grep -qF 'export PATH="/symlink/root/bin:$PATH"' "$TMP/profile-target"

chmod 600 "$TMP/profile-target"
check_true "a mode-preserving reconciliation succeeds" \
    add_to_profile "$TMP/profile-link" zsh /mode/root/bin
check_eq "600" "$(file_mode "$TMP/profile-target")" "profile permissions are preserved"
check_eq "0" "$(find "$TMP" -maxdepth 1 -name 'profile-target.filecraft.*' | wc -l | tr -d ' ')" \
    "symlink replacement leaves no temporary file"

printf 'prefix\n%s\nold\n%s\nsuffix' "$BEGIN_MARKER" "$END_MARKER" > "$TMP/no-final-newline"
printf 'prefix\n%s\nexport PATH="/newline/root/bin:$PATH"\n%s\nsuffix' \
    "$BEGIN_MARKER" "$END_MARKER" > "$TMP/no-final-newline.expected"
check_true "a profile without a final newline is reconciled" \
    add_to_profile "$TMP/no-final-newline" zsh /newline/root/bin
check_true "reconciliation preserves a missing final newline exactly" \
    cmp -s "$TMP/no-final-newline.expected" "$TMP/no-final-newline"

printf 'prefix\n%s\nold\n%s\nsuffix\n' "$BEGIN_MARKER" "$END_MARKER" > "$TMP/one-final-newline"
printf 'prefix\n%s\nexport PATH="/newline/root/bin:$PATH"\n%s\nsuffix\n' \
    "$BEGIN_MARKER" "$END_MARKER" > "$TMP/one-final-newline.expected"
check_true "a profile with one final newline is reconciled" \
    add_to_profile "$TMP/one-final-newline" zsh /newline/root/bin
check_true "reconciliation preserves one final newline exactly" \
    cmp -s "$TMP/one-final-newline.expected" "$TMP/one-final-newline"

printf 'prefix\n%s\nold\n%s\nsuffix\n\n\n' "$BEGIN_MARKER" "$END_MARKER" > "$TMP/blank-final-lines"
printf 'prefix\n%s\nexport PATH="/newline/root/bin:$PATH"\n%s\nsuffix\n\n\n' \
    "$BEGIN_MARKER" "$END_MARKER" > "$TMP/blank-final-lines.expected"
check_true "a profile with final blank lines is reconciled" \
    add_to_profile "$TMP/blank-final-lines" zsh /newline/root/bin
check_true "reconciliation preserves every final blank line" \
    cmp -s "$TMP/blank-final-lines.expected" "$TMP/blank-final-lines"
check_eq "0" "$(find "$TMP" -maxdepth 1 -name '*.filecraft.*' | wc -l | tr -d ' ')" \
    "newline reconciliation leaves no temporary files"

# A missing fish config directory is created rather than failing.
check_true "fish config is created" \
    add_to_profile "$TMP/fish/config.fish" fish /home/tester/.cargo/bin
check_true "fish gets fish syntax" \
    grep -qF 'fish_add_path $HOME/.cargo/bin' "$TMP/fish/config.fish"

# A dry run explains itself and writes nothing.
DRY_RUN=yes
: > "$TMP/dry"
add_to_profile "$TMP/dry" zsh /home/tester/.cargo/bin > "$TMP/dry.out"
check_eq "0" "$(wc -c < "$TMP/dry" | tr -d ' ')" "dry run wrote nothing"
check_true "dry run said what it would do" grep -q 'would append' "$TMP/dry.out"
cp "$TMP/zshrc" "$TMP/dry-replace"
cp "$TMP/dry-replace" "$TMP/dry-replace.before"
add_to_profile "$TMP/dry-replace" zsh /another/root/bin > "$TMP/dry-replace.out"
check_true "replacement dry run writes nothing" \
    cmp -s "$TMP/dry-replace.before" "$TMP/dry-replace"
check_true "replacement dry run describes the replacement" \
    grep -q 'would replace' "$TMP/dry-replace.out"
DRY_RUN=no

# --------------------------------------------------- the script as a whole

HOME="$REAL_HOME"
check_true "--help exits zero" bash "$ROOT/install.sh" --help
check_true "--help documents the export line" \
    bash -c "bash '$ROOT/install.sh' --help | grep -qF 'export PATH=\"\$HOME/.cargo/bin:\$PATH\"'"
check_false "an unknown option is refused" bash "$ROOT/install.sh" --nope
check_true "--dry-run --no-build changes nothing" \
    bash "$ROOT/install.sh" --dry-run --no-build --yes

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
