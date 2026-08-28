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
