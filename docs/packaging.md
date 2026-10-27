# Packaging and releases

How the Linux, Windows and macOS releases are made and published. Building and
checking Yapfast for development is in [building](building.md).

## Linux AppImage

`scripts/build-appimage` makes the Linux release in `target/dist/linux`:
`Yapfast-VERSION-x86_64.AppImage`, its `.zsync` file, the source snapshot and
`SHA256SUMS.txt`. All four go on the release page. AppImageUpdate, and AppImage
managers such as Gear Lever, find a newer release through the `.zsync` file,
named by the update information in the runtime:
`gh-releases-zsync|yapfast|yapfast|latest|Yapfast-*-x86_64.AppImage.zsync`.
The first build compiles every dependency in the Nix sandbox; packing takes
about 10 seconds after that.

Nix builds the binary, `nix/package.nix`'s Discord-only release build, and
`nix/appimage.nix` puts it in an AppDir with the desktop file, the icons and
the AppStream metainfo, `packaging/linux/io.github.yapfast.Yapfast.metainfo.xml`.
It sets the binary's interpreter to `/lib64/ld-linux-x86-64.so.2` and removes
Nix's RUNPATH, so the binary runs on the host's loader, glibc and libstdc++, as
one built on a distribution would. That's what lets the host's GPU driver,
Mesa or NVIDIA's, load into it: a driver needs the glibc it was built for, and
NVIDIA's must match the kernel module, so neither can come in the AppImage.
Nothing else is bundled either. Besides the C and C++ runtimes, the binary
links only GLib, PulseAudio's client, fontconfig and FreeType, which every
desktop has, and it loads EGL, Vulkan, Wayland, X11 and PipeWire at runtime.

The derivation fails if the binary links anything else, or asks for a glibc
newer than 2.39 or a libstdc++ newer than GCC 14's. That covers Ubuntu 24.04,
Fedora 40, Debian 13 and anything newer. nixpkgs' glibc is newer than that, so
a nixpkgs update can raise what the binary asks for. Raise the floors in
`nix/appimage.nix` only knowing which distributions that drops. Older ones,
such as Ubuntu 22.04, would need a build against their own glibc.

`scripts/package-linux.py`, which runs in the development shell, with
squashfs-tools and zsync added from the shell's nixpkgs, adds the
licenses and notices, `THIRD-PARTY.json`, `SOURCE.txt` and `BUILD.json` to
`usr/share/doc/yapfast`, as the Windows package has them, through the helpers
in `scripts/release_package.py` that both packaging scripts use. Skia's
prebuilt Linux archive carries only Skia's own license, so the notices of its
third-party code come from the pinned Skia source,
`nix build path:./nix#skia-source`. Then it packs the AppDir with mksquashfs
(zstd level 19, 1 MiB blocks, fixed owners and times, so the same AppDir packs
to the same file) behind AppImage's official type-2 runtime, pinned by hash.
The runtime is static: it mounts the image with FUSE 3's `fusermount3` and
needs no libfuse, and `--appimage-extract-and-run` runs it without FUSE at all.
The flake sees only the files Git tracks, so add a new file under `nix/` or
`packaging/` before building.

Screen sharing hasn't been tried from the AppImage.

## Arch Linux packages

`packaging/arch/PKGBUILD` builds the Discord-only release from source with
Arch's toolchain, as `nix/package.nix` builds it: `cd packaging/arch && makepkg
-si`. It clones the tag of its `pkgver`, which `scripts/set-version` sets to a
stable version, so run `scripts/set-version 0.2.0` before committing and
tagging `v0.2.0`. The tag's commit holds the PKGBUILD, which can't hold its own
checksum, so that source has none; the libwebrtc and Skia archives keep theirs.
Arch's `llvm21` is a later 21.x than Nix's, which is why
`nix/prepare-skia-native.py` takes any LLVM 21. Nothing builds this PKGBUILD
automatically: build it in a clean `archlinux:base-devel` container before a
release, about 6 minutes from cold.

The releases carry a package of their own binary instead. `scripts/package-arch`,
run after `scripts/build-appimage`, packs the AppImage's binary, desktop file,
icon, metainfo and notices into `yapfast-VERSION-1-x86_64.pkg.tar.zst` beside
it, through `packaging/arch/release.PKGBUILD`, and adds it to
`SHA256SUMS.txt`. It's the AppImage's binary, which runs on the host's loader
and libraries, so the two packages can't differ. The script runs makepkg in an
`archlinux:base-devel` container with Docker, then installs the package there
and runs `yapfast --smoke-test`, in about 20 seconds. A nightly's version
becomes `0.2.0nightly.20261020.5`, which pacman sorts after 0.1.0 and before
0.2.0, as SemVer sorts the nightly; with a dot before `nightly`, pacman would
sort it after 0.2.0. `sudo pacman -U` installs the file or its link; there's no
repository, so `pacman -Syu` doesn't update it.

## macOS app

`scripts/build-macos [discord|full]` makes `target/dist/Yapfast.app` for
Apple silicon, and beside it in `target/dist/macos` the app's ZIP,
`Yapfast-VERSION-macos-arm64.zip` (`-full-preview` before `-macos` for the
full flavour), the source snapshot and `SHA256SUMS.txt`. It runs in
`scripts/macos-env`, sources `scripts/release-env aarch64-apple-darwin`, so
the binary names no folder of the machine that built it, and builds with
`cargo build --release --target aarch64-apple-darwin`. macOS's `strip`
leaves a proc macro's library unloadable, so the release profile strips only
what ships, not build scripts and proc macros (`Cargo.toml`). A cold build
takes about 7 minutes on an M4 Pro; packing takes seconds.

`scripts/package-macos.py` lays the bundle out as macOS expects it:
`Contents/Info.plist`, from `packaging/macos/Info.plist` with the version from
`Cargo.toml` (without a nightly's suffix, as the plist's versions are
numbers only); `Contents/MacOS/yapfast`; and `Contents/Resources` with the
icon, the license, `SOURCE.txt`, `BUILD.json`, `THIRD-PARTY.json` and the
dependencies' licenses and notices, through `scripts/release_package.py` as
on Linux and Windows. rust-skia's prebuilt macOS Skia carries only Skia's own
license, so the notices of the code linked into it, expat, libjpeg-turbo,
libpng, Wuffs and zlib, come from their source at the revisions
`nix/flake.nix` pins: the script downloads them once into
`~/Library/Caches/yapfast-build`, checks each against the flake's hash and
keeps only the license files.

The Info.plist names the app `io.github.yapfast.Yapfast`, the Linux app ID,
and carries the sentences macOS shows when Yapfast first asks for the
microphone, the camera and other apps' sound (Go Live's, a Core Audio process
tap). Screen recording, notifications and input monitoring take no sentence.
Permissions belong to the app that asks: started from the `.app`, Yapfast
asks for them itself; started from a terminal, macOS asks for the terminal.

The script signs the bundle ad hoc (`codesign --sign -`), which Apple silicon
requires of every program, without the hardened runtime, which only
notarization needs, so no entitlements either. macOS ties permissions and
Keychain access to the signature, and an ad hoc one changes with every build,
so each build asks again. To keep them, sign with a stable identity, such as
a self-signed code-signing certificate made in Keychain Access, named in
`YAPFAST_CODESIGN_IDENTITY`. Neither is notarized, so Gatekeeper stops the
downloaded app until it's allowed in System Settings › Privacy & Security, or
its quarantine is removed (`xattr -dr com.apple.quarantine Yapfast.app`).
`ditto -c -k --keepParent` zips the bundle as Finder would.

Opened from Finder or the Dock, Yapfast's output goes to `/dev/null`, so it
writes its log to `~/Library/Logs/Yapfast/yapfast.log` instead, where Console
finds it (`src/main.rs`). Run from a terminal, or with its output going to a
file or pipe, it logs there as on Linux.

## Nightly releases

Releases are built locally, but a nightly of `main` builds on GitHub: Actions,
Nightly, Run workflow. `.github/workflows/nightly.yml` names it after the next
minor version, the day (UTC) and the workflow's run number, such as
`v0.2.0-nightly.20261020.5`, and titles it as the stable releases are, with the
commit: `Yapfast 0.2.0-nightly.20261020.5 (396abe2)`. The run number puts two
nightlies of one day in order, as a commit hash wouldn't, and gives a second
run on one commit a tag of its own. `scripts/set-version` writes that version
into `Cargo.toml` and `Cargo.lock` before building, so the app reports it and
the packages and `SOURCE.txt` name it. One job runs `scripts/build-appimage`
and `scripts/package-arch`, and another `scripts/build-windows` in the Windows shell, with the Windows Skia
build, cargo-xwin's MSVC files and the compiled Rust dependencies
(`Swatinem/rust-cache`) cached between runs. The Linux build has no cache: it
compiles in the Nix sandbox, which can't reach one. The last job publishes the
AppImage, its `.zsync` file, the Arch package, the Windows ZIP, the source snapshot and
`SHA256SUMS.txt` as a prerelease on that commit. Wine doesn't start on the
runner, so there's no installer. The release's notes list the commits since the
previous nightly, or the latest release, each with its author's GitHub account:
features and fixes by their Conventional Commit type, the rest folded away
(`.github/nightly-notes.jq`). Every nightly stays on the releases page. Both
builds zip the source snapshot with the commit's time on every file, so they
make the same file, and the release carries one. A prerelease is never
`latest`, so the AppImage's update information leaves stable installs on stable
releases, and takes a nightly AppImage to the latest stable release, not the
next nightly.

## The app icon

`packaging/icon.svg` is Yapfast's icon: the logo in marigold, its dots in
on-accent ink, so it reads on a light taskbar as well as a dark one. The UI
keeps drawing `ui/icons/logo.svg`.

On Linux the window's app ID is `io.github.yapfast.Yapfast` (`src/icon.rs`).
Wayland desktops show a window's icon only through the desktop entry of that
name, `packaging/linux/io.github.yapfast.Yapfast.desktop`;
`scripts/install-desktop-entry [EXECUTABLE]` installs it and the icon for
this user, running `target/release/yapfast` unless told otherwise. The
AppImage installs nothing, so Yapfast run from one writes the entry and the
icon to `~/.local/share` itself before it opens its window
(`src/desktop_entry.rs`), with Exec and TryExec naming the AppImage. It
rewrites them when the AppImage has moved, and leaves them alone when a package
has its own entry in `XDG_DATA_DIRS`, or an AppImage manager such as Gear Lever
made one that runs the same file. On X11 the window also carries the icon
itself. Notifications name the entry, so the notification server shows the
same name and icon.

On Windows, `build.rs` compiles `packaging/windows/yapfast.rc` with `llvm-rc`
and links the icon into `yapfast.exe`, for Explorer and shortcuts; the window
loads it from there for its title bar and the taskbar, each at the size
Windows draws it. After changing `icon.svg`, run `scripts/make-windows-icon`
to draw `yapfast.ico` again.

On macOS the bundle's `packaging/macos/yapfast.icns` is the Dock's and
Finder's icon. `scripts/make-macos-icon` draws it from `icon.svg` again, at
each size from 16 to 1024 pixels, with the logo filling 824 of 1024 pixels as
Apple's icon grid has it, so it matches other apps' icons in the Dock. Run
from outside a bundle, such as from `target/`, Yapfast gives the Dock the
same logo itself: AppKit draws `icon.svg`, its view box grown to the same
grid, at the Dock's size (`src/icon.rs`).
