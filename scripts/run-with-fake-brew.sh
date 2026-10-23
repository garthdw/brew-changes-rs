#!/usr/bin/env bash
# Builds and runs brew-changes against a fake `brew` and a local fake GitHub,
# so every changelog path can be tried without anything being outdated.
# Your real keymap.json/theme.json are still used. For local testing only.
#
# Packages shown:
#   md-tool       formula, CHANGELOG.md in the repo (code block, table, links)
#   release-tool  formula, no changelog file, so GitHub releases
#   cask-app      cask with a "3.1,1234" style version, GitHub releases
#   curl          formula with an HTML changelog from changelog_config.json;
#                 this one fetches the real https://curl.se/changes.html
#   no-source     cask with no changelog source
#
# `gh` is never run: GITHUB_TOKEN is set to a dummy, which also keeps your
# real token away from the fake server.
#
# Usage: scripts/run-with-fake-brew.sh [brew-changes args...]
#   e.g. scripts/run-with-fake-brew.sh --theme "Solarized (light)"
# Set FAKE_BREW_UPGRADE_FAIL=1 to make in-place upgrades fail.

set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
command -v python3 &>/dev/null || {
  echo "python3 is required for the fake GitHub server" >&2
  exit 1
}

dir="$(mktemp -d "${TMPDIR:-/tmp}/brew-changes-fake.XXXXXX")"
server_pid=""
cleanup() {
  if [ -n "$server_pid" ]; then
    kill "$server_pid" &>/dev/null || true
    # Reaping it stops bash printing "Terminated" after the TUI exits.
    wait "$server_pid" &>/dev/null || true
  fi
  rm -rf "$dir"
}
trap cleanup EXIT

mkdir -p "$dir/bin"

cat >"$dir/outdated.json" <<'EOF'
{
  "formulae": [
    {"name": "md-tool", "installed_versions": ["1.0.0"], "current_version": "2.0.0"},
    {"name": "release-tool", "installed_versions": ["2.0.0"], "current_version": "2.1.0"},
    {"name": "curl", "installed_versions": ["8.9.1"], "current_version": "8.10.0"}
  ],
  "casks": [
    {"name": "cask-app", "installed_versions": ["3.0,1200"], "current_version": "3.1,1234"},
    {"name": "no-source", "installed_versions": ["1.0"], "current_version": "1.1"}
  ]
}
EOF

cat >"$dir/info-formula.json" <<'EOF'
{"formulae": [
  {"full_name": "md-tool", "homepage": "https://github.com/fake/md-tool"},
  {"full_name": "release-tool", "homepage": "https://github.com/fake/release-tool"},
  {"full_name": "curl", "homepage": "https://curl.se"}
]}
EOF

cat >"$dir/info-cask.json" <<'EOF'
{"casks": [
  {"token": "cask-app", "full_token": "cask-app", "homepage": "https://github.com/fake/cask-app"},
  {"token": "no-source", "full_token": "no-source", "homepage": "https://example.com"}
]}
EOF

cat >"$dir/bin/brew" <<EOF
#!/bin/sh
dir='$dir'
echo "\$*" >>"\$dir/brew-calls.log"
case "\$1 \$3" in
  outdated*) cat "\$dir/outdated.json" ;;
  "info --formula") cat "\$dir/info-formula.json" ;;
  "info --cask") cat "\$dir/info-cask.json" ;;
  upgrade*)
    sleep 1
    if [ -n "\${FAKE_BREW_UPGRADE_FAIL:-}" ]; then
      echo "Error: fake upgrade failure for \$*" >&2
      exit 1
    fi
    echo "fake brew: \$*" ;;
  *) exit 1 ;;
esac
EOF
chmod +x "$dir/bin/brew"

cat >"$dir/github.py" <<'EOF'
import base64
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

port_file, log_file = sys.argv[1], sys.argv[2]

CHANGELOG = """# Changelog

## 2.0.0

### Added

- A **new** `--fast` flag; see [the docs](https://example.com/docs).
- Config can now be written as TOML:

```toml
[server]
port = 8080
verbose = true
```

| Platform | Status |
| -------- | ------ |
| macOS    | yes    |
| Linux    | yes    |

> Upgrading from 1.x? Run `md-tool migrate` first.

## 1.5.0

- Fixed the middle bug ([#12](https://github.com/fake/md-tool/pull/12)).

```rust
fn main() {
    println!("fixed");
}
```

## 1.0.0

- Initial release
"""


def contents(text):
    return {"content": base64.b64encode(text.encode()).decode(), "encoding": "base64"}


def release(tag, date, body, prerelease=False):
    return {"tag_name": tag, "published_at": date, "body": body, "prerelease": prerelease}


ROUTES = {
    "/repos/fake/md-tool/contents/": [
        {"name": "README.md", "type": "file"},
        {"name": "CHANGELOG.md", "type": "file"},
    ],
    "/repos/fake/md-tool/contents/CHANGELOG.md": contents(CHANGELOG),
    "/repos/fake/release-tool/contents/": [{"name": "README.md", "type": "file"}],
    "/repos/fake/release-tool/releases": [
        release("v2.1.0", "2026-10-01T00:00:00Z", "## What's changed\n\n- Faster startup\n- `--json` output\n\n```sh\nrelease-tool --json | jq .\n```"),
        release("v2.0.1", "2026-09-01T00:00:00Z", "- Fixed a crash on empty input"),
        release("v2.0.0", "2026-08-01T00:00:00Z", "- Big rewrite"),
    ],
    "/repos/fake/cask-app/contents/": [],
    "/repos/fake/cask-app/releases": [
        release("v3.1", "2026-10-10T00:00:00Z", "### Cask App 3.1\n\n- Dark mode\n- *Italic* and ~~struck~~ notes"),
        release("v3.0", "2026-09-10T00:00:00Z", "- First 3.x release"),
    ],
}


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = ROUTES.get(urlsplit(self.path).path)
        self.send_response(404 if body is None else 200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps({"message": "Not Found"} if body is None else body).encode())

    def log_message(self, format, *args):
        with open(log_file, "a") as log:
            log.write(format % args + "\n")


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(port_file, "w") as port:
    port.write(str(server.server_port))
server.serve_forever()
EOF

python3 -I "$dir/github.py" "$dir/port" "$dir/github.log" &
server_pid=$!

for _ in $(seq 50); do
  [ -s "$dir/port" ] && break
  sleep 0.1
done
[ -s "$dir/port" ] || {
  echo "fake GitHub server did not start" >&2
  exit 1
}

cargo build --quiet --manifest-path "$repo/Cargo.toml" --bin brew-changes

env -u GH_TOKEN \
  PATH="$dir/bin:$PATH" \
  GITHUB_TOKEN=fake-token \
  BREW_CHANGES_GITHUB_API="http://127.0.0.1:$(cat "$dir/port")" \
  "$repo/target/debug/brew-changes" "$@"
