# brew-changes

A Homebrew external command that adds `brew changes`: before you upgrade, see
what actually changed.

`brew outdated` tells you *that* packages are outdated. `brew changes` tells
you *what's in* the update: an interactive list of every outdated formula and
cask, where you can expand any of them in place to read the changelog section
between your installed version and the new one, falling back to GitHub
Releases. Then upgrade everything, or just the one you're looking at.

## Install

```sh
brew install garthdw/tap/brew-changes
```

This installs a prebuilt binary. No Rust toolchain is needed.

## Usage

```sh
brew changes
```

This will:

1. Find every outdated formula and cask (`brew outdated`).
2. Resolve each one's changelog source: a curated override, or a GitHub
   repository detected from its homepage or source URL.
3. Open an interactive list. Nothing is fetched from GitHub yet, so this is
   instant even with many outdated packages.
4. Expanding a package fetches its `CHANGELOG`, `CHANGES`, `HISTORY` or `NEWS`
   file (`.md`, `.rst` or no extension), or its GitHub releases, and renders
   the relevant section as Markdown in place. HTML changelog pages are
   converted to Markdown.
5. Upgrade everything, or just the highlighted package.

### Keybindings

| Key | Action |
| --- | --- |
| `↑`/`↓`, `K`/`J` | Move the cursor |
| `k`/`j` | Scroll the changelog when the highlighted package is expanded, otherwise move the cursor |
| `Enter` | Expand/collapse the highlighted package's changelog |
| `o` | Open the highlighted package's homepage (or GitHub repo) in your browser |
| `g` | Open the highlighted package's GitHub repo in your browser |
| `c` | Open the highlighted package's changelog in your browser |
| `a` | Quit and upgrade all outdated packages |
| `u` | Upgrade just the highlighted package, in place |
| `?` | Show the help/about popup |
| `q`, `Ctrl+C` | Quit without upgrading |

If an in-place upgrade fails, the row shows `[upgrade failed]`; expand it to
see brew's output.

#### Customizing keybindings

Override any binding in `$XDG_CONFIG_HOME/brew-changes/keymap.json` or, if
`XDG_CONFIG_HOME` isn't set, `~/Library/Application Support/brew-changes/keymap.json`
on macOS (`~/.config/brew-changes/keymap.json` on Linux):

```json
{
  "main": {
    "x": "quit",
    "q": null
  }
}
```

Each entry maps a key name (`"j"`, `"K"`, `"ctrl+e"`, `"esc"`, `"enter"`,
`"up"`, ...) to an action name, or to `null` to unbind it. Valid action
names: `quit`, `move-up`, `move-down`, `move-up-or-scroll-up`,
`move-down-or-scroll-down`, `toggle-expand`, `open-homepage`, `open-github`,
`open-changelog`, `upgrade-all`, `upgrade-current`, `help`. The footer hints
and the help popup reflect your active bindings.

## GitHub API rate limits

`brew changes` talks to the GitHub API directly. Unauthenticated requests are
limited to 60/hour, which can be tight if you review a lot of changelogs. Set
a token to raise that limit:

```sh
export GITHUB_TOKEN=ghp_...   # or GH_TOKEN
```

If you have the [`gh`](https://cli.github.com) CLI installed and authenticated
(`gh auth login`), `brew changes` uses its token automatically when neither
environment variable is set.

## Development

```sh
cargo test
cargo run                                          # the TUI
cargo run --bin validate-changelogs -- 100         # top 100 formulae
cargo run --bin validate-changelogs -- 100 -cask   # top 100 casks
cargo run --bin validate-changelogs -- -outdated   # this machine's outdated packages
scripts/downgrade-for-testing.sh jq bat            # create real outdated packages
```

## Releasing

Publishing a GitHub Release (with a `vX.Y.Z` tag) triggers
[`.github/workflows/release.yml`](.github/workflows/release.yml), which runs
[GoReleaser](https://goreleaser.com) with `cargo zigbuild` to build macOS and
Linux binaries (arm64 + amd64), attach them to the release, and commit the
regenerated `Formula/brew-changes.rb` to `garthdw/homebrew-tap`.
