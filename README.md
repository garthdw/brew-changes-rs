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
brew install garthdw/tap/brew-changes-rs
```

This installs a prebuilt binary. No Rust toolchain is needed. It conflicts
with the Go `brew-changes` formula, since both provide `brew changes`; switch
between them with `brew unlink brew-changes && brew link brew-changes-rs`.

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
| `t` | Pick a theme, previewed live (not saved) |
| `?` | Show the help/about popup; `l` there shows the bundled syntax and theme licences |
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
`open-changelog`, `upgrade-all`, `upgrade-current`, `pick-theme`, `help`. The footer hints
and the help popup reflect your active bindings.

#### Customizing colours

Themes live in `theme.json`, next to `keymap.json`:

```json
{
  "theme": { "dark": "base16-ocean.dark", "light": "InspiredGitHub" },
  "themes": {
    "InspiredGitHub": { "link": { "fg": "#0366d6", "underline": false } },
    "Solarized (dark)": { "border": "#586e75", "selected": { "reversed": true } },
    "mine": { "syntax": "base16-mocha.dark", "accent": "#ff79c6" }
  }
}
```

`theme` is either one theme name, used regardless of the terminal background
(and skipping the background query), or a `dark`/`light` pair picked by the
detected background. Either half may be left out to keep its default.
`brew changes --theme <name>` overrides it for one run, and `t` opens a
picker that previews each theme live; neither is saved.

The built-in themes are bat's, via [two-face](https://crates.io/crates/two-face):
`base16-ocean.dark` (default dark), `base16-eighties.dark`,
`base16-mocha.dark`, `base16-ocean.light`, `InspiredGitHub` (default light),
`Solarized (dark)` and `Solarized (light)` first, then `ansi`, `base16`,
`base16-256` (these three use the terminal's own colours), `Catppuccin
Frappe`, `Catppuccin Latte`, `Catppuccin Macchiato`, `Catppuccin Mocha`,
`Coldark-Cold`, `Coldark-Dark`, `DarkNeon`, `Dracula`, `GitHub`,
`gruvbox-dark`, `gruvbox-light`, `1337`, `Monokai Extended` (plus `Bright`,
`Light` and `Origin`), `Nord`, `OneHalfDark`, `OneHalfLight`, `Sublime
Snazzy`, `TwoDark` and `zenburn`.
Each brings its own palette to the list, headings, links and code, matching
its syntax highlighting (versions use the theme's number colour, for
example). The first seven are hand-tuned; the rest are taken from each
theme's own colours for strings, numbers, functions, comments and so on. Entries under `themes` override colours of a built-in theme, or
define a new one whose `syntax` names the built-in it starts from and whose
palette it inherits. Themes can be configured without being selected.

Style keys: `accent`, `border`, `muted`, `error`, `version`, `heading`,
`title`, `code`, `code-block`, `link`, `header` (the "Outdated packages:"
heading and the name in the help popup), and `selected` (added to the
highlighted row). `header` and `selected` are bold by default. Each is either a foreground colour or an
object with any of `fg`, `bg`, `bold`, `dim`, `italic`, `underline`,
`strikethrough` and `reversed`; only the attributes given change, so
`{"bold": false}` keeps the default colour. Colours are names (`"red"`,
`"lightblue"`), hex (`"#rrggbb"`) or a 256-colour index (`"39"`). Unknown keys
or bad values are reported at startup and ignored.

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
scripts/run-with-fake-brew.sh [--theme <name>]    # the TUI with fake brew/GitHub (no real upgrades)
```

## Releasing

Publishing a GitHub Release (with a `vX.Y.Z` tag) triggers
[`.github/workflows/release.yml`](.github/workflows/release.yml), which builds
macOS arm64 and Linux (arm64 + amd64, musl) binaries in parallel, then runs
[GoReleaser](https://goreleaser.com) to attach them to the release and commit
the regenerated `Formula/brew-changes-rs.rb` to `garthdw/homebrew-tap`.
