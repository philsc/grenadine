#!/usr/bin/env bash
# Runs the server with dictation on, with the Whisper models that Bazel
# downloaded. The first three arguments, from the BUILD file, are the
# server and the models; the rest go to the server.
#
# The models' paths are resolved because the server keeps its arguments
# when an update execs the new build: the runfiles tree is replaced by later
# builds, while the models stay in Bazel's repository cache. The server's
# path is resolved only to keep the logged command line tidy.
set -euo pipefail

server="$(realpath "$1")"
model="$(realpath "$2")"
preview_model="$(realpath "$3")"
shift 3

exec "$server" \
    --whisper-model="$model" \
    --whisper-preview-model="$preview_model" \
    "$@"
