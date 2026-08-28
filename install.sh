#!/usr/bin/env bash
#
# install.sh - build filecraft, then make sure your shell can find it.
#
# `cargo install` puts the binary in $CARGO_HOME/bin (normally
# ~/.cargo/bin), which a macOS zsh does not search unless something put it
# on PATH. The install then looks like it worked and the next `filecraft`
# is `zsh: command not found`. This script closes that gap: it installs,
# checks PATH and your shell's startup file, and offers to add the one
# line that fixes it.
#
# Every edit is guarded by markers, so re-running changes nothing twice.
#
# Usage:  ./install.sh [--yes] [--dry-run] [--no-path] [--link[-dir DIR]]
# Tests:  scripts/install_test.sh sources this file with
#         FILECRAFT_INSTALL_LIB=1, which defines the functions below and
#         returns without installing anything.

set -euo pipefail

BEGIN_MARKER='# >>> filecraft install >>>'
END_MARKER='# <<< filecraft install <<<'

# ---------------------------------------------------------------- output

is_tty() { [ -t 1 ]; }

if is_tty && [ -z "${NO_COLOR:-}" ]; then
    C_BOLD=$'\033[1m'; C_DIM=$'\033[2m'; C_WARN=$'\033[33m'
    C_OK=$'\033[32m'; C_ERR=$'\033[31m'; C_OFF=$'\033[0m'
else
    C_BOLD=''; C_DIM=''; C_WARN=''; C_OK=''; C_ERR=''; C_OFF=''
fi

step() { printf '%s==>%s %s\n' "$C_BOLD" "$C_OFF" "$*"; }
info() { printf '    %s\n' "$*"; }
note() { printf '    %s%s%s\n' "$C_DIM" "$*" "$C_OFF"; }
good() { printf '%sok%s  %s\n' "$C_OK" "$C_OFF" "$*"; }
warn() { printf '%swarning:%s %s\n' "$C_WARN" "$C_OFF" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$C_ERR" "$C_OFF" "$*" >&2; exit 1; }

# ------------------------------------------------------------ pure logic
#
# The functions in this section touch nothing but their arguments (and
# $HOME), which is what makes scripts/install_test.sh able to assert on
# them. src/pathcheck.rs is the same decision in Rust, for `filecraft
# update`; the two must agree on what "on PATH" means.

# Splitting on IFS is how both functions below read a path apart, and
# unquoted expansion also globs - which would turn an entry like /opt/*
# into whatever happens to be on disk. Each turns globbing off around its
# split and back on only if it was on to begin with. The `set -f` has to
# happen in the function's own shell, so this cannot be factored into a
# helper called through $( ): that runs in a subshell and changes nothing.

# Expand a leading ~, $HOME or ${HOME}, then drop empty and "." path
# components so two spellings of one directory compare equal. Written
# without ${var//x/y} because bash 3.2 - the bash macOS ships - does not
# unescape a slash in the replacement.
normalize_dir() {
    local dir="$1"
    case "$dir" in
        '~')            dir="$HOME" ;;
        '~/'*)          dir="$HOME/${dir#\~/}" ;;
        '$HOME')        dir="$HOME" ;;
        '$HOME/'*)      dir="$HOME/${dir#\$HOME/}" ;;
        '${HOME}')      dir="$HOME" ;;
        '${HOME}/'*)    dir="$HOME/${dir#\$\{HOME\}/}" ;;
    esac

    local absolute=no
    case "$dir" in /*) absolute=yes ;; esac

    local out='' part reglob=''
    case "$-" in *f*) ;; *) reglob=yes; set -f ;; esac
    local IFS=/
    for part in $dir; do
        case "$part" in ''|.) continue ;; esac
        if [ -z "$out" ]; then out="$part"; else out="$out/$part"; fi
    done
    unset IFS
    if [ -n "$reglob" ]; then set +f; fi

    if [ "$absolute" = yes ]; then
        printf '/%s' "$out"
    elif [ -z "$out" ]; then
        printf '.'
    else
        printf '%s' "$out"
    fi
}

# Does this PATH value tell a shell to search DIR? Entry-by-entry and
# exact: "~/.cargo" must not count as "~/.cargo/bin".
path_contains_dir() {
    local want entry found=1 reglob=''
    want="$(normalize_dir "$1")"
    case "$-" in *f*) ;; *) reglob=yes; set -f ;; esac
    local IFS=:
    for entry in $2; do
        [ -n "$entry" ] || continue
        if [ "$(normalize_dir "$entry")" = "$want" ]; then
            found=0
            break
        fi
    done
    unset IFS
    if [ -n "$reglob" ]; then set +f; fi
    return "$found"
}

# Classify $SHELL (a path such as /bin/zsh, or a login shell's "-zsh").
shell_kind() {
    local name="${1##*/}"
    case "${name#-}" in
        zsh)      printf 'zsh' ;;
        bash|sh)  printf 'bash' ;;
        fish)     printf 'fish' ;;
        *)        printf 'other' ;;
    esac
}

# The startup file that shell reads for interactive sessions.
profile_for() {
    case "$1" in
        zsh)  printf '%s/.zshrc' "$HOME" ;;
        bash) printf '%s/.bashrc' "$HOME" ;;
        fish) printf '%s/.config/fish/config.fish' "$HOME" ;;
        *)    printf '%s/.profile' "$HOME" ;;
    esac
}

# Write a directory the way it should appear in a startup file: under the
# home directory it becomes $HOME/..., so the line survives being copied
# to another machine or another user.
portable_dir() {
    local dir
    dir="$(normalize_dir "$1")"
    local home
    home="$(normalize_dir "$HOME")"
    case "$dir" in
        "$home")    printf '$HOME' ;;
        "$home"/*)  printf '$HOME/%s' "${dir#"$home"/}" ;;
        *)          printf '%s' "$dir" ;;
    esac
}

# The line that puts DIR on PATH, in that shell's syntax.
export_line_for() {
    local kind="$1" dir
    dir="$(portable_dir "$2")"
    case "$kind" in
        fish) printf 'fish_add_path %s' "$dir" ;;
        *)    printf 'export PATH="%s:$PATH"' "$dir" ;;
    esac
}

# Does this startup file already put DIR on PATH? Matches our own marked
# block, a hand-written export, and rustup's `. "$HOME/.cargo/env"` -
# any of which means there is nothing to add.
profile_has_dir() {
    local file="$1" dir="$2"
    [ -f "$file" ] || return 1
    local portable stripped
    portable="$(portable_dir "$dir")"
    dir="$(normalize_dir "$dir")"
    stripped="${dir#"$(normalize_dir "$HOME")"/}"
    local line
    while IFS= read -r line; do
        case "${line%%#*}" in *[![:space:]]*) ;; *) continue ;; esac
        case "$line" in
            *"$dir"*|*"$portable"*|*"~/$stripped"*) return 0 ;;
            *cargo/env*) [ "${dir##*/}" = bin ] && return 0 ;;
        esac
    done < "$file"
    return 1
}

# ------------------------------------------------------------- installing

repo_root() { cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P; }

# Where `cargo install` writes. CARGO_INSTALL_ROOT wins over CARGO_HOME,
# which is the order cargo itself uses; getting it wrong would send the
# advice at a directory the binary never lands in.
cargo_bin_dir() {
    printf '%s/bin' "${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}"
}

run() {
    if [ "$DRY_RUN" = yes ]; then
        note "would run: $*"
        return 0
    fi
    "$@"
}

# Append the PATH line under markers, creating the file if needed. Two
# runs leave one block: an existing block means there is nothing to do.
add_to_profile() {
    local file="$1" kind="$2" dir="$3"
    if [ -f "$file" ] && grep -qF "$BEGIN_MARKER" "$file"; then
        return 1
    fi
    local line
    line="$(export_line_for "$kind" "$dir")"
    if [ "$DRY_RUN" = yes ]; then
        note "would append to $file:"
        note "    $line"
        return 0
    fi
    mkdir -p -- "$(dirname -- "$file")"
    {
        printf '\n%s\n' "$BEGIN_MARKER"
        printf '%s\n' "$line"
        printf '%s\n' "$END_MARKER"
    } >> "$file"
    return 0
}

ask_yes() {
    [ "$ASSUME_YES" = yes ] && return 0
    [ -t 0 ] || return 1
    local reply
    printf '    %s [y/N] ' "$1"
    read -r reply || return 1
    case "$reply" in y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
}

usage() {
    cat <<'EOF'
install.sh - build filecraft and make sure your shell can find it

USAGE:
  ./install.sh [OPTIONS]

OPTIONS:
  -y, --yes         edit the shell startup file without asking
  -n, --dry-run     print every change without making one
      --no-path     install only; never touch a startup file
      --link        also symlink the binary into ~/.local/bin
      --link-dir D  also symlink the binary into D
      --no-build    skip the build; only check and fix PATH
  -h, --help        show this help

It runs `cargo install --path . --locked --force`, which puts `filecraft`
in $CARGO_INSTALL_ROOT or $CARGO_HOME (normally ~/.cargo/bin). If it is not on
your PATH and your shell's startup file does not add it, this offers to
append:

  export PATH="$HOME/.cargo/bin:$PATH"

Re-running is safe: the edit is fenced by markers and made only once.
EOF
}

main() {
    DRY_RUN=no
    ASSUME_YES=no
    EDIT_PATH=yes
    DO_BUILD=yes
    LINK_DIR=''

    while [ $# -gt 0 ]; do
        case "$1" in
            -y|--yes)     ASSUME_YES=yes ;;
            -n|--dry-run) DRY_RUN=yes ;;
            --no-path)    EDIT_PATH=no ;;
            --no-build)   DO_BUILD=no ;;
            --link)       LINK_DIR="$HOME/.local/bin" ;;
            --link-dir)
                shift
                [ $# -gt 0 ] || die "--link-dir needs a directory"
                LINK_DIR="$1"
                ;;
            --link-dir=*) LINK_DIR="${1#--link-dir=}" ;;
            -h|--help)    usage; return 0 ;;
            *)            die "unknown option '$1'; try ./install.sh --help" ;;
        esac
        shift
    done

    local root bin_dir binary
    root="$(repo_root)"
    bin_dir="$(cargo_bin_dir)"
    binary="$bin_dir/filecraft"

    if [ "$DO_BUILD" = yes ]; then
        command -v cargo >/dev/null 2>&1 ||
            die "cargo is not installed; get a Rust toolchain from https://rustup.rs"
        [ -f "$root/Cargo.toml" ] ||
            die "no Cargo.toml next to install.sh; run it from a filecraft clone"
        step "Building and installing filecraft"
        run cargo install --path "$root" --locked --force
    fi

    if [ "$DRY_RUN" = no ] && [ "$DO_BUILD" = yes ] && [ ! -x "$binary" ]; then
        die "cargo reported success but $binary is not there"
    fi

    step "Checking PATH"
    local kind profile line
    kind="$(shell_kind "${SHELL:-}")"
    profile="$(profile_for "$kind")"
    line="$(export_line_for "$kind" "$bin_dir")"

    if path_contains_dir "$bin_dir" "${PATH:-}"; then
        good "$bin_dir is on your PATH"
    elif [ "$EDIT_PATH" = no ]; then
        warn "$bin_dir is not on your PATH; add this to $profile yourself:"
        info "$line"
    elif profile_has_dir "$profile" "$bin_dir"; then
        good "$profile already adds $bin_dir; open a new terminal to pick it up"
    else
        info "$bin_dir is not on your PATH, so \`filecraft\` will not run by name."
        info "Fix it by adding this line to $profile:"
        info "  $line"
        if ask_yes "Add it now?"; then
            if add_to_profile "$profile" "$kind" "$bin_dir"; then
                if [ "$DRY_RUN" = no ]; then
                    good "added to $profile"
                    info "run: source $profile     (or just open a new terminal)"
                fi
            else
                good "$profile already has a filecraft block; nothing to add"
            fi
        else
            warn "left $profile alone; add the line above yourself, or re-run with --yes"
        fi
    fi

    if [ -n "$LINK_DIR" ]; then
        step "Linking into $LINK_DIR"
        run mkdir -p -- "$LINK_DIR"
        run ln -sf -- "$binary" "$LINK_DIR/filecraft"
        if path_contains_dir "$LINK_DIR" "${PATH:-}"; then
            good "$LINK_DIR/filecraft -> $binary (and $LINK_DIR is on your PATH)"
        else
            warn "$LINK_DIR is not on your PATH either; add it the same way"
        fi
    fi

    step "Done"
    if [ "$DRY_RUN" = yes ]; then
        note "dry run: nothing was installed or edited"
    elif [ -x "$binary" ]; then
        info "$("$binary" --version 2>/dev/null || printf 'filecraft')  ($binary)"
        info "try: filecraft            # open the current directory"
        info "     filecraft --help"
    fi
}

# Sourced by scripts/install_test.sh: define the functions, install nothing.
if [ -n "${FILECRAFT_INSTALL_LIB:-}" ]; then
    return 0 2>/dev/null || exit 0
fi

main "$@"
