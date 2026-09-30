# Contributing to Grab

## Architecture

Single-binary GTK 4 / libadwaita app (`src/`):

| File | Role |
|------|------|
| `main.rs` | Entry point, GSettings schema dir bootstrap |
| `application.rs` | Lifecycle (`startup`/`activate`/`open`/`shutdown`), actions, accels, About/Shortcuts dialogs |
| `window.rs` | Main window (header bar, `ViewStack` empty/list, failure `Banner`), New Download dialog |
| `download.rs` | `DownloadManager`: queue, tokio engine (`EngineMsg` channel), pause/resume/cancel/retry, atomic JSON persistence |
| `preferences.rs` | `AdwPreferencesDialog` bound to GSettings |

Threading rule: network I/O runs on a dedicated tokio runtime and only sends
`EngineMsg` over a channel; every GTK touch happens in `glib::spawn_future_local`
(main thread). GSettings keys live in `data/io.github.linuxuser67.Grab.gschema.xml`.

## Commands

```bash
cargo fmt --check          # must be clean
cargo clippy --all-targets -- -D warnings   # must be zero warnings
cargo test -- --test-threads=1   # serial: parallel runs abort when one
                                  # test's loop polls another's glib source
                                  # (thread-bound futures, shared context)
```

Disk cleanup: `target/` and `build/` are safe to delete anytime (regenerable).
Keep `.flatpak-builder/` — it caches vendored crate downloads, so the
next Flatpak build skips the ~10 min re-download.

CI (`.github/workflows/ci.yml`) runs all three on push/PR.

## UI changes

Follow the GNOME HIG: `AdwApplicationWindow` + `HeaderBar`, `AdwPreferencesDialog`
for settings, symbolic icons with tooltips *and* accessible labels, `AdwToast`
(+ Undo) for reversible actions, `AdwBanner` for persistent states. See
the HIG: <https://developer.gnome.org/hig/>.

## Releases (maintainers, Flatpak-only)

```bash
# 1. Bump version in Cargo.toml + metainfo, commit, push, tag (e.g. v1.2.0)
# 2. Regen vendored sources only if Cargo.lock gained/lost crates:
python3 build-aux/gen-cargo-sources.py
# 3. Smoke-test locally if you like (incremental, no force-clean, no bundle):
flatpak run org.flatpak.Builder --user --install \
  build build-aux/io.github.linuxuser67.Grab.json
flatpak run io.github.linuxuser67.Grab --help
# 4. Publish the release; CI (.github/workflows/flatpak.yml) clean-builds
#    the bundle from the tag and attaches Grab.flatpak itself:
gh release create <tag> --title "Grab <tag>" --notes "..."
# Rebuild/attach again later without a new release:
gh workflow run flatpak.yml -f tag=<tag>
```

## Test builds (no local building)

Need a bundle from a branch to try out? Build it in CI, download the
artifact — no `--force-clean`, nothing touches releases:

```bash
gh workflow run flatpak-test.yml -f ref=<branch>
# then: Actions tab -> Flatpak test bundle run -> Artifacts -> Grab.flatpak
# (kept 14 days; runners are ephemeral, so speed comes from the SDK cache)
# Pull requests touching code/data/packaging build one automatically.
```
