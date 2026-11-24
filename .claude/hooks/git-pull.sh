#!/bin/sh
# SessionStart hook: bring the project up to date with origin so every
# session (Mac, Linux, ...) works on the latest code. Never blocks: local
# changes, no network or a failed pull only produce a note.
cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0
if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "git: local changes present, did not pull"
    exit 0
fi
before=$(git rev-parse HEAD)
if GIT_TERMINAL_PROMPT=0 git -c http.lowSpeedLimit=1000 -c http.lowSpeedTime=10 pull --ff-only --quiet 2>/dev/null; then
    after=$(git rev-parse HEAD)
    if [ "$before" != "$after" ]; then
        echo "git: pulled updates:"
        git log --oneline "$before..$after"
    fi
else
    echo "git: pull failed (offline or diverged), working on the local version"
fi
exit 0
