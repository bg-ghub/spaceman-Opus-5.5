# spaceman Opus 5.5 <img src="spaceman.png" width="28"/>

A performance fork of [salihgerdan/spaceman](https://github.com/salihgerdan/spaceman).
The treemap still grows live while scanning, but scans are much faster and use far less memory.

| Windows, warm cache | upstream v0.2.1 | this fork |
|---|---|---|
| `C:\Program Files` (266k items) | 22.3 s, 143 MB | 0.48 s, 24 MB |
| `C:\Windows` (411k items) | 37.4 s, 307 MB | 4.4 s, 64 MB |
| Layout of a 1920x1080 view | 10.4 ms | 0.8 ms |

What changed:
- Own breadth-first parallel scanner instead of jwalk. On Windows the file size comes from the
  directory listing, so there is no extra metadata call per file.
- Scan results are applied to the tree in one short locked batch every 15 ms, so the UI and the
  scanner barely wait on each other.
- 32-byte tree nodes: names in one arena, paths rebuilt on demand, children as a sibling list.
- The layout skips everything too small to draw, and hover highlighting has its own layer.

Folder sizes count file contents only, not the folders' own NTFS index blocks.

---

## Original README

[![Downloads](https://img.shields.io/github/downloads/salihgerdan/spaceman/total)](https://github.com/salihgerdan/spaceman/releases)
[![Downloads (Latest)](https://img.shields.io/github/downloads/salihgerdan/spaceman/latest/total)](https://github.com/salihgerdan/spaceman/releases/latest)

Treemap disk usage analyzer: *In search of lost space* (a.k.a. wata-analyzer)  
![Screenshot](screenshot.png?raw=true)

## Features
- [x] Multi-threaded fast scans (upstream uses the jwalk library, this fork its own scanner)
- [x] Linux-first, cross-platform
- [x] Live display of scan results, responsive UI
- [x] Zoom into sub-directories
- [x] Single portable executable with no dependencies
## Planned
- [ ] More themes
## Install  
**To build**: The only command necessary to build is `cargo build --release`.  
You can also simply grab an executable from the [Releases](https://github.com/salihgerdan/spaceman/releases) section.  
There is an [AUR package `spaceman-git`](https://aur.archlinux.org/packages/spaceman-git) available for Arch Linux (i.e. run `yay install spaceman-git`).  
### Mac tells me this is trash
> "SpaceMan" is damaged and can't be opened. You should move it to the Trash.

As this is an unsigned app, you will have to remove the quarantine after installation with the following command in the terminal.

```sh
xattr -d com.apple.quarantine /Applications/SpaceMan.app
```
## Usage
Click the button on the left of the titlebar, and choose a directory to scan. You can also provide a directory to scan as a command line argument.

Don't forget to empty the system trash after using the "trash" option in order to reclaim the space.
## News! (exciting)
- ~~I am now looking for new icon ideas/submissions~~ (I have received a submission, working on it)
- Version 0.2.0 was a near-rewrite of the app and we switched from using GTK4 to Iced as our UI library. As I wanted to make this a portable application, this finally allowed us to avoid shipping .DLL files or asking to install dependencies.
## Acknowledgements
[SpaceSniffer](http://www.uderzo.it/main_products/space_sniffer/) for inspiration  
[Bruls, Huizing, van Wijk, "Squarified Treemaps"](https://www.win.tue.nl/~vanwijk/stm.pdf) and [TreeMonger](https://github.com/alanbernstein/treemonger) which I referenced for the squarified treemap algorithm  
[dua-cli](https://github.com/Byron/dua-cli/) for reference on using the jwalk library
