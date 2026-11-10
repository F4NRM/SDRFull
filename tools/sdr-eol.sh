#!/usr/bin/env bash
# Keep each modified file's line endings as they are in HEAD.
#
# The Windows original lives outside the repo; this one works on Linux/macOS
# and does the same job: a file whose HEAD blob is CRLF stays CRLF, one that is
# LF stays LF. Run it before `git add` after any tool that rewrites a file.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
python3 - <<'PY'
import subprocess, os

def blob(path):
    try:
        return subprocess.check_output(["git", "show", f"HEAD:{path}"], stderr=subprocess.DEVNULL)
    except subprocess.CalledProcessError:
        return None

files = subprocess.check_output(["git", "diff", "--name-only", "HEAD"], text=True).splitlines()
for path in files:
    if not os.path.isfile(path):
        continue
    head = blob(path)
    if head is None:
        continue
    data = open(path, "rb").read()
    head_crlf = b"\r\n" in head
    data_crlf = b"\r\n" in data
    if head_crlf and not data_crlf:
        open(path, "wb").write(data.replace(b"\r\n", b"\n").replace(b"\n", b"\r\n"))
        print(f"crlf: {path}")
    elif not head_crlf and data_crlf:
        open(path, "wb").write(data.replace(b"\r\n", b"\n"))
        print(f"lf:   {path}")
PY
git diff --numstat
