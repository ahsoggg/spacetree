# SpaceTree

[English](README.en.md) · [Русский](README.md)

A fast disk-space analyzer for Windows, in the spirit of WizTree and WinDirStat: folder tree, treemap,
per-type statistics, search, and hints about what is safe to delete — plus a history of your scans
("what grew since last week?") and duplicate detection by actual file content.

> The interface is currently in Russian only. Everything described below works the same regardless.

![Overview: folder tree and treemap](docs/screenshots/02-overview.png)

*All screenshots use made-up data (`SpaceTree.exe --demo`) — no real file names.*

## Features

- **Two scan engines.** On NTFS with administrator rights SpaceTree reads the MFT directly — a whole
  drive in seconds (about 17 s for 2 million files on a spinning disk, versus minutes for a directory walk).
  Without admin rights it falls back to a parallel directory walk.
- **Overview:** sortable tree (name, size, file count, date) and a treemap with animated zoom, colored by
  file type or by age, with highlighting of a chosen extension.
- **What grew.** Every scan saves a small snapshot (sizes of the bigger folders). The *What grew* tab compares
  the current scan with an earlier one — by default the one from about a week ago — and points at the culprit
  folder: `+12 GB in AppData\Local\Temp`. A parent folder whose growth is explained by one subfolder is folded
  into that subfolder, so the list is not full of every ancestor on the way down.
- **Real duplicate search.** Files of equal size → hash of the first 64 KiB → full BLAKE3 hash. Only files that
  still have a possible twin are read further, so most files are never opened. The newest copy of each set is
  kept; hover a row to see which one. Runs on demand, can be cancelled.
- **Safe-to-delete hints:** temp files, browser and app caches, crash dumps, `node_modules` / `target`,
  package-manager caches, old installers, the recycle bin, … with *safe / likely safe / review* levels.
  Tick what you want and delete to the recycle bin or permanently.
- **Deleted file recovery (NTFS, administrator rights).** The "♻ Восстановление" button scans the MFT for deleted
  files with names and paths and rates the chance of recovery from the cluster bitmap; "deep search" carves JPG,
  PNG, PDF, ZIP/Office, MP4 and more by signature in free space. Preview, NTFS-compression support, restoring only
  to another drive.
- **Deleting from the app** (`Del` → recycle bin, `Shift+Del` → permanent) with confirmation; the tree and
  statistics update instantly, no rescan.
- **Hard links instead of deleting duplicates:** "Схлопнуть в жёсткие ссылки" (collapse into hard links) in the cleanup tab
  keeps every name in place but stores the data once (NTFS, same volume).
- **LZX compression (Windows Compact)** from the folder / file context menu: NTFS's built-in transparent compression,
  usually −30…50 % on programs, games and assets; programs run as before. Never follows junctions or symlinks;
  "remove compression" undoes it.
- **Hidden NTFS streams (ADS):** a tab and pink highlight on the map for files with megabytes or gigabytes hidden in
  `file.txt:name` that Explorer cannot see. A single stream can be deleted (MFT scan, administrator rights).
- **Live mode:** the "Live" switch at the bottom follows the NTFS change journal (USN) — the map glides to its new
  layout when something starts writing gigabytes (MFT scan of a whole drive, administrator rights).
- **Orphaned data:** `AppData` folders without an installed program and leftovers of removed Steam / Epic Games games —
  separate "review" groups in the cleanup tab; a guess from names, nothing is ticked for you.
- Search by name (`Ctrl+F`), largest files, file types, CSV export, recent paths, arrow-key navigation,
  `F5` to rescan.
- **Auto-update** from GitHub Releases with SHA-256 and Ed25519 signature verification (can be turned off).
- **Diagnostics for bug reports:** an error log and a crash report written locally (see below).

| What grew | Duplicates by content |
| --- | --- |
| ![What grew](docs/screenshots/04-growth.png) | ![Duplicates](docs/screenshots/06-duplicates.png) |

| Cleanup hints | File types |
| --- | --- |
| ![Cleanup](docs/screenshots/05-cleanup.png) | ![File types](docs/screenshots/03-types.png) |

## Install

Download `SpaceTree-Setup.exe` from [Releases](https://github.com/ahsoggg-creator/spacetree/releases/latest)
and run it. No administrator rights needed — it installs to `%LOCALAPPDATA%\Programs\SpaceTree` and adds:

- a Start menu shortcut (Windows search finds it by "spacetree");
- **"Scan in SpaceTree"** in the context menu of folders, folder backgrounds and drives
  (Windows 11: under "Show more options", `Shift+F10`);
- an optional desktop shortcut and an entry in "Installed apps" for uninstalling.

There is also a portable build — just `SpaceTree.exe`; it can install itself later from the settings (⚙).

### Code signing

Release executables are signed with a **self-signed** certificate, `CN=ahsoggg`
([`certs/ahsoggg.cer`](certs/ahsoggg.cer), SHA-256 fingerprint
`83A7AA2EA6B3787AD9573F4EB7D3A588C081235368F0079284DB5C1E6EFF0828`).

A self-signed certificate is not issued by a certificate authority, so be clear about what it does and does
not give you:

- It lets you check that a download is byte-for-byte what the author signed, and that two releases come from
  the same key (compare the fingerprint above).
- It does **not** make Windows SmartScreen happy: until a certificate is trusted on your machine, Windows still
  shows "unknown publisher". If you trust the author, you can import it for your user with
  `powershell -ExecutionPolicy Bypass -File scripts\trust_cert.ps1` (it prints the fingerprint first).

The private key stays on the author's machine; `*.pfx` files are git-ignored.

## Diagnostics and privacy

SpaceTree sends nothing anywhere except the update check to GitHub. For troubleshooting it keeps, locally:

- `%APPDATA%\SpaceTree\logs\spacetree.log` — scans, failed deletions, update problems (rotated at 512 KB);
- `%APPDATA%\SpaceTree\logs\crashes\crash-<time>.txt` — written if the program crashes: version, panic
  message and location, backtrace and the tail of the log (which may mention paths of files that could not
  be read or deleted — look before you send it). On the next start SpaceTree tells you where the report is.

Settings (⚙) → *Diagnostics* opens that folder or copies the diagnostics text to the clipboard — attach it
to a bug report. Scan snapshots live in `%APPDATA%\SpaceTree\snapshots` (a text file per scan; the last 40
per scanned location are kept) and can be switched off in the settings.

## Build

Needs Rust (stable, edition 2024) and MSVC.

```powershell
cargo build --release            # target\release\SpaceTree.exe and spacetree-cli.exe
cargo test --workspace
powershell -ExecutionPolicy Bypass -File scripts\make_cert.ps1   # once: create the signing certificate
powershell -ExecutionPolicy Bypass -File scripts\release.ps1     # tests, build, sign, artifacts in dist\
```

Layout:

- `crates/core` — tree model, scanners (directory walk, NTFS MFT), statistics, cleanup rules, duplicate
  search, snapshots and diffs, CSV;
- `crates/app` — the egui GUI, installer, auto-update, Explorer integration, diagnostics, demo mode;
- `crates/cli` — `spacetree-cli scan C:\ --top 20 --cleanup --dupes --csv out.csv`.

The same exe acts as the installer when it is named `SpaceTree-Setup.exe` (or started with `--install`) and
as the uninstaller with `--uninstall`.

Regenerate the screenshots (made-up data, nothing real is scanned or written):

```powershell
SpaceTree.exe --screenshots docs\screenshots
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this work, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.
