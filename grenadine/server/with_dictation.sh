#!/usr/bin/env bash
# Runs the server with dictation on, with the Whisper models that Bazel
# downloaded. The first three arguments, from the BUILD file, are the
# server and the models; the rest go to the server.
#
# The server keeps its arguments when an update execs the new build, so the
# models' paths have to outlive this build. Bazel's own copies don't: the
# runfiles tree is replaced by later builds, and Bazel deletes repo contents
# cache entries that no build used for a while, which an update's build of
# the server alone doesn't. So the models are kept in grenadine's data
# directory, hard-linked when it's on the same file system. The server's
# path is resolved only to keep the logged command line tidy.
set -euo pipefail
# So that a failure in keep, which runs in $(...), stops the wrapper.
shopt -s inherit_errexit

server="$(realpath "$1")"
dir="${XDG_DATA_HOME:-$HOME/.local/share}/grenadine/whisper"
mkdir -p "$dir"

# Puts the model at $1 into $dir, unless it's there already, and prints
# the path of the kept file.
keep() {
    local src dst tmp
    src="$(realpath "$1")"
    dst="$dir/$(basename "$src")"
    if [[ ! "$src" -ef "$dst" ]] && ! cmp -s "$src" "$dst"; then
        tmp="$(mktemp "$dst.XXXXXX")"
        if ! ln -f "$src" "$tmp" 2>/dev/null && ! cp "$src" "$tmp"; then
            rm -f "$tmp"
            return 1
        fi
        mv -f "$tmp" "$dst"
    fi
    echo "$dst"
}

model="$(keep "$2")"
preview_model="$(keep "$3")"
shift 3

exec "$server" \
    --whisper-model="$model" \
    --whisper-preview-model="$preview_model" \
    "$@"
