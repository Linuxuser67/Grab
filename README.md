# Grab

A download manager for GNOME. Built with GTK 4 and libadwaita.

- [Screenshots](#screenshots)
- [Install](#install)
  - [Cookies from Browser in the Flatpak](#cookies-from-browser-in-the-flatpak)
- [Browser extension](#browser-extension)
- [Notes for packagers](#notes-for-packagers)
- [License](#license)

## Screenshots

<table>
  <tr>
    <td align="center">
      <img src="screenshots/dark-live.png" alt="Grab main window with a live stream recording and a download resuming" width="400">
      <br>
      <em>Live stream recording alongside active downloads</em>
    </td>
    <td align="center">
      <img src="screenshots/dark-downloads.png" alt="Grab main window with an active video download and block map" width="400">
      <br>
      <em>Active downloads with per-block progress maps</em>
    </td>
  </tr>
  <tr>
    <td align="center">
      <img src="screenshots/dark-torrent.png" alt="Grab main window with an active torrent download" width="400">
      <br>
      <em>Torrent download with segmented filter (All / Active / Queued / Downloaded)</em>
    </td>
    <td align="center">
      <img src="screenshots/dark-new-download.png" alt="New Download card with URL, file name and media format picker" width="400">
      <br>
      <em>New Download card with inline media format picker</em>
    </td>
  </tr>
  <tr>
    <td align="center">
      <picture>
        <source media="(prefers-color-scheme: dark)" srcset="screenshots/dark-empty.png">
        <img src="screenshots/light-empty.png" alt="Grab empty state" width="400">
      </picture>
      <br>
      <em>Empty state</em>
    </td>
    <td align="center">
      <img src="screenshots/light-prefs-network.png" alt="Grab preferences, Network tab" width="400">
      <br>
      <em>Preferences: simultaneous downloads, connections, retries, speed limit</em>
    </td>
  </tr>
  <tr>
    <td align="center">
      <img src="screenshots/light-prefs-torrent.png" alt="Grab preferences, Torrent tab" width="400">
      <br>
      <em>Preferences: seeding, DHT, and peer limit</em>
    </td>
    <td></td>
  </tr>
</table>

## Install

[![Get it on FlatPark](assets/get-it-on-flatpark.png)](https://flatpark.org/apps/io.github.linuxuser67.Grab/)

From FlatPark (recommended, gets updates via `flatpak update`):

```bash
flatpak remote-add --if-not-exists flatpark https://dl.flatpark.org/flatpark.flatpakrepo
flatpak install flatpark io.github.linuxuser67.Grab
```

Or manually from the [latest release](https://github.com/Linuxuser67/Grab/releases):

```bash
flatpak install --user Grab.flatpak
```

### Cookies from Browser in the Flatpak

The sandbox ships without access to browser profiles. When you pick a browser
under Preferences → Authentication → Cookies from Browser and its profile is
unreachable, Grab shows the exact `flatpak override` command to grant
read-only access — or run the equivalent yourself:

```bash
flatpak override --user --filesystem=~/.config/<browser-dir>:ro io.github.linuxuser67.Grab
```

Undo with `flatpak override --user --reset io.github.linuxuser67.Grab`.

### Media tool installs

Grab fetches its media tools (yt-dlp, ffmpeg, quickjs) from their upstream
GitHub releases on first run, and Preferences → Check for Updates keeps them
current. Each download is verified against the SHA-256 digest published in the
same GitHub release before it is installed; a missing digest refuses the
install.

That digest detects corrupted or truncated downloads, not a compromised
release — it shares the release's trust root. If you need a separate trust
root, install the tools from your distribution instead.

## Browser extension

Send browser downloads and links straight to Grab with the
[Grab browser extension](https://github.com/Linuxuser67/Grab-browser-extension)
(for Chromium-based browsers and Firefox): automatic download interception with a
configurable minimum size, toolbar button, `Alt+G` shortcut, and a right-click
menu.

[![Get it for Firefox](assets/get-it-on-firefox.png)](https://addons.mozilla.org/en-US/firefox/addon/grab-extension/)

The extension tries the native messaging host first (direct launch, no tab),
falling back to `grab://` URLs if the host isn't installed.

To install the host (recommended, avoids the browser's external-protocol prompt):

```bash
grab --install-browser-host --chromium-id <extension-id>
```

Find the extension ID on your browser's extensions page (Developer mode).
For Firefox, the host is registered automatically via the manifest.

## Notes for packagers

Flatpak-only.

```bash
flatpak-builder --user --install build build-aux/io.github.linuxuser67.Grab.json
```

## License

Grab is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License as published by the Free Software
Foundation, version 3 only. See [LICENSE](LICENSE) for the full text.
