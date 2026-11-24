#!/bin/sh
# Start vid_recorder (macOS / Linux) with the latest version.
#
# - In a git checkout with Rust installed (development machine): git pull,
#   rebuild if anything changed, run.
# - Otherwise (installed from a release archive): download the newest
#   GitHub release if it differs from ./VERSION, unpack it here, run.
#
# Arguments are passed on, e.g. ./run.sh record-test 5
# No network or a failed update: starts the current version.
# Everything is inside main() so the shell has read the whole script before
# an update overwrites this file.

REPO=mrdudas/vid_recorder

source_mode() {
    if ! git diff --quiet || ! git diff --cached --quiet; then
        echo "update: local changes, not pulling" >&2
    elif ! GIT_TERMINAL_PROMPT=0 git -c http.lowSpeedLimit=1000 -c http.lowSpeedTime=10 pull --ff-only --quiet; then
        echo "update: git pull failed, starting the current version" >&2
    fi
    # Incremental: does nothing if no source changed.
    if ! cargo build --release --quiet; then
        echo "build failed" >&2
        [ -x target/release/vid_recorder ] || exit 1
        echo "starting the previous build" >&2
    fi
    BIN=target/release/vid_recorder
}

release_mode() {
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64) asset=vid_recorder-macos-arm64 ;;
        Darwin-x86_64) asset=vid_recorder-macos-x86_64 ;;
        Linux-x86_64) asset=vid_recorder-linux-x86_64 ;;
        *) echo "update: no release for $(uname -s) $(uname -m)" >&2 ;;
    esac
    BIN=./vid_recorder
    [ -n "$asset" ] || return
    latest=$(curl -fsS --max-time 10 "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null |
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
    if [ -z "$latest" ]; then
        echo "update: cannot reach GitHub, starting the current version" >&2
        return
    fi
    [ "$latest" = "$(cat VERSION 2>/dev/null)" ] && return
    echo "update: downloading $latest" >&2
    tmp=$(mktemp -d) || return
    if curl -fsSL --max-time 300 -o "$tmp/a.tar.gz" \
        "https://github.com/$REPO/releases/download/$latest/$asset.tar.gz" &&
        tar -xzf "$tmp/a.tar.gz" -C "$tmp" && rm "$tmp/a.tar.gz" && cp -R "$tmp"/. .; then
        echo "update: now $latest" >&2
    else
        echo "update: download failed, starting the current version" >&2
    fi
    rm -rf "$tmp"
}

main() {
    cd "$(dirname "$0")" || exit 1
    if [ -d .git ] && command -v cargo >/dev/null 2>&1; then
        source_mode
    else
        release_mode
    fi
    [ -x "$BIN" ] || { echo "vid_recorder not found ($BIN)" >&2; exit 1; }
    exec "$BIN" "$@"
}

main "$@"
exit
