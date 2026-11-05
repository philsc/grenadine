#!/usr/bin/env bash
# Regenerates packages.txt, the .deb lockfile behind @chrome_deps.
set -euo pipefail

if [[ -z "${BUILD_WORKSPACE_DIRECTORY:-}" ]]; then
    echo "error: BUILD_WORKSPACE_DIRECTORY is unset; run via bazel run" >&2
    exit 1
fi

# The snapshot.ubuntu.com timestamp the packages resolve against.
SNAPSHOT="20251201T000000Z"

# The libraries come from jammy so the same set runs on the Ubuntu 22.04
# development machines and on the ubuntu:24.04 remote executors, whose newer
# glibc still satisfies the jammy builds' symbol versions.
IMAGE="ubuntu:22.04"

# The packages holding the shared libraries Chrome loads from the host.
PACKAGES=(
    fontconfig
    fonts-dejavu-core
    fonts-liberation2
    libasound2
    libatk-bridge2.0-0
    libatk1.0-0
    libatspi2.0-0
    libdbus-1-3
    libexpat1
    libgbm1
    libglib2.0-0
    libnspr4
    libnss3
    libudev1
    libx11-6
    libxcb1
    libxcomposite1
    libxdamage1
    libxext6
    libxfixes3
    libxkbcommon0
    libxrandr2
)

# What the executor's base image already ships, so the lockfile holds only
# what apt would add there. The jammy image's own package set can't decide
# this: the two bases ship different libraries.
BASE_PACKAGES="$(
    docker run --rm ubuntu:24.04 dpkg-query -W '-f=${binary:Package}\n' \
        | sed 's/:amd64$//'
)"

uris="$(
    docker run --rm -e "SNAPSHOT=$SNAPSHOT" -e "BASE_PACKAGES=$BASE_PACKAGES" \
        "$IMAGE" bash -c '
        set -euo pipefail
        sed -i "s|http://archive.ubuntu.com/ubuntu/|https://snapshot.ubuntu.com/ubuntu/$SNAPSHOT/|" /etc/apt/sources.list
        sed -i "s|http://security.ubuntu.com/ubuntu/|https://snapshot.ubuntu.com/ubuntu/$SNAPSHOT/|" /etc/apt/sources.list
        # The bare image has no CA certificates for the snapshot host. That is
        # acceptable here: apt still verifies the GPG signatures of the
        # archive, and the sha256s in packages.txt come from downloads the
        # host makes with TLS verification.
        apt-get update -qq -o Acquire::https::Verify-Peer=false
        # The apt resolver run against an empty dpkg status picks one
        # alternative of each a | b | c dependency, as a real install would.
        # --print-uris download then reports URIs even for packages this
        # resolution image has installed.
        comm -23 \
            <(apt-get install -s --no-install-recommends \
                -o Dir::State::status=/dev/null "$@" \
                | grep -oE "^Inst [a-zA-Z0-9][a-zA-Z0-9+.-]*" \
                | cut -d" " -f2 | sort -u) \
            <(echo "$BASE_PACKAGES" | sort -u) \
            | xargs apt-get --print-uris -qq download
    ' _ "${PACKAGES[@]}"
)"

out="$BUILD_WORKSPACE_DIRECTORY/third_party/chrome_deps/packages.txt"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

{
    echo "# The pinned .debs behind @chrome_deps: the shared libraries Chrome"
    echo "# loads from the host, plus their jammy dependencies, minus what the"
    echo "# ubuntu:24.04 base image already ships."
    echo "# Regenerate with: bazel run //third_party/chrome_deps:update"
    while read -r url file _; do
        url="${url#\'}"
        url="${url%\'}"
        curl -fsSL "$url" -o "$tmp/$file"
        sha256="$(sha256sum "$tmp/$file" | cut -d' ' -f1)"
        echo "$sha256  $url"
        rm "$tmp/$file"
    done <<<"$uris"
} | {
    head -4
    sort
} >"$out"

echo "wrote $out"
