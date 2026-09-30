# Grab

A download manager for GNOME. Built with GTK 4 and libadwaita.

## Screenshots

<p align="center">
  <img src="screenshots/light-active.png" alt="Grab main window with a live capture and a torrent downloading" width="720">
  <br>
  <em>Active downloads: indeterminate live capture plus segmented torrent with block map</em>
</p>

<p align="center">
  <img src="screenshots/dark-active.png" alt="Grab main window in dark mode with active and queued sections" width="720">
  <br>
  <em>Active and queued sections with expanded segment map (dark mode)</em>
</p>

<p align="center">
  <img src="screenshots/light-dialog-vod.png" alt="Media details dialog for a YouTube video with quality picker" width="720">
  <br>
  <em>Media details: quality picker with per-format sizes</em>
</p>

<p align="center">
  <img src="screenshots/light-dialog-live.png" alt="Media details dialog for a Twitch live stream with HLS format" width="720">
  <br>
  <em>Media details for a live stream: HLS variant picker</em>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="screenshots/dark-prefs-downloads.png">
    <img src="screenshots/light-prefs-downloads.png" alt="Grab preferences, Downloads tab" width="720">
  </picture>
  <br>
  <em>Preferences: destination, power, and notifications</em>
</p>

<p align="center">
  <img src="screenshots/light-prefs-network.png" alt="Grab preferences, Network tab" width="720">
  <br>
  <em>Preferences: simultaneous downloads, connections, retries, speed limit</em>
</p>

<p align="center">
  <img src="screenshots/light-prefs-torrent.png" alt="Grab preferences, Torrent tab" width="720">
  <br>
  <em>Preferences: seeding, DHT, and peer limit</em>
</p>

<p align="center">
  <img src="screenshots/dark-empty.png" alt="Grab empty state in dark mode" width="720">
  <br>
  <em>Empty state (dark mode)</em>
</p>

## Install

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

## Notes for packagers

Flatpak-only.

```bash
flatpak-builder --user --install build build-aux/io.github.linuxuser67.Grab.json
```

## License

Grab is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License as published by the Free Software
Foundation, version 3 only. See [LICENSE](LICENSE) for the full text.
