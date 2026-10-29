#!/usr/bin/env python3
"""Package the Linux release as an AppImage: nix/appimage.nix's AppDir, with
the dependencies' licenses and notices added, packed behind the AppImage
runtime. Beside it go the .zsync file AppImageUpdate reads, which the release
must carry too, and the source snapshot.

scripts/build-appimage runs it inside the development shell."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

from release_package import (FLAVOURS, ROOT, SKIA_VERSION, UI_FRAMEWORK, build_hashes, build_identity, cargo_metadata,
                             dependency_notices, digest, local_skia_archive, names_home, source_notice,
                             source_snapshot)

TARGET = "x86_64-unknown-linux-gnu"
FEATURES = FLAVOURS["discord"]["features"]


def nix_build(installable):
    return Path(subprocess.check_output(["nix", "build", "--no-link", "--print-out-paths", installable],
                                        cwd=ROOT, text=True, timeout=7200).strip())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/dist/linux")
    args = parser.parse_args()
    # The flake sees only files Git tracks; a new one must be added first.
    parts = nix_build(".#appdir")
    skia_source = nix_build("path:./nix#skia-source")
    metadata, version = cargo_metadata(TARGET, FEATURES)
    skia_archive = local_skia_archive(os.environ["SKIA_BINARIES_URL"])
    webrtc = Path(os.environ["LK_CUSTOM_WEBRTC"])
    output = args.output
    output.mkdir(parents=True, exist_ok=True)
    base = f"Yapfast-{version}"
    image = output / f"{base}-x86_64.AppImage"
    source_path = output / f"{base}-source.zip"
    build = json.loads((parts / "build.json").read_text())
    with tempfile.TemporaryDirectory(prefix="yapfast-linux-package-") as temporary:
        temporary = Path(temporary)
        rtc = source_snapshot(metadata, base, temporary, source_path)
        if rtc:
            raise SystemExit("The AppImage is the Discord flavour, which has no AGPL dependencies")

        appdir = temporary / "AppDir"
        shutil.copytree(parts / "AppDir", appdir, symlinks=True)
        for directory in [appdir, *(path for path in appdir.rglob("*") if path.is_dir() and not path.is_symlink())]:
            directory.chmod(0o755)
        executable = appdir / "usr/bin/yapfast"
        contents = executable.read_bytes()
        if names_home(contents):
            raise SystemExit(f"{executable} names {Path.home()}; it should come from the Nix build")
        del contents

        doc = appdir / "usr/share/doc/yapfast"
        doc.mkdir(parents=True)
        shutil.copy2(ROOT / "LICENSE", doc / "LICENSE")
        (doc / "SOURCE.txt").write_text(source_notice(version, "", source_path, rtc), encoding="utf-8")
        dependencies = dependency_notices(metadata, doc, doc / "licenses", skia_archive, skia_source, webrtc)
        (doc / "THIRD-PARTY.json").write_text(json.dumps(dependencies, indent=2) + "\n", encoding="utf-8")
        lock = json.loads((ROOT / "flake.lock").read_text())["nodes"]["nixpkgs"]["locked"]
        build = {
            **build_identity(version, "discord", TARGET, FEATURES),
            "built_by": "nix build .#appdir",
            "nixpkgs_revision": lock["rev"],
            "ui_framework": UI_FRAMEWORK,
            "skia_version": SKIA_VERSION,
            **build_hashes(skia_archive, executable, source_path),
            **build,
        }
        (doc / "BUILD.json").write_text(json.dumps(build, indent=2) + "\n", encoding="utf-8")

        # Fixed owners and times, so the same AppDir packs to the same file.
        # mksquashfs takes the times from SOURCE_DATE_EPOCH, which the
        # development shell sets, or its options, not both.
        filesystem = temporary / "AppDir.squashfs"
        environment = {name: value for name, value in os.environ.items() if name != "SOURCE_DATE_EPOCH"}
        subprocess.run(["mksquashfs", appdir, filesystem, "-noappend", "-quiet", "-no-progress",
                        "-root-owned", "-no-xattrs", "-mkfs-time", "0", "-all-time", "0",
                        "-comp", "zstd", "-Xcompression-level", "19", "-b", "1M"],
                       env=environment, check=True, timeout=1800)
        with image.open("wb") as stream:
            for part in (parts / "runtime", filesystem):
                with part.open("rb") as piece:
                    shutil.copyfileobj(piece, stream)
        image.chmod(0o755)

    offset = subprocess.check_output([image, "--appimage-offset"], text=True, timeout=30).strip()
    if int(offset) != (parts / "runtime").stat().st_size:
        raise SystemExit(f"{image} finds its filesystem at {offset}, not after the runtime")
    update = subprocess.check_output([image, "--appimage-updateinformation"], text=True, timeout=30).strip()
    if update != build["update_information"]:
        raise SystemExit(f"{image} has the update information {update!r}")
    zsync = output / f"{image.name}.zsync"
    subprocess.run(["zsyncmake", "-u", image.name, "-o", zsync, image], check=True, timeout=600,
                   stdout=subprocess.DEVNULL)
    hashes = output / "SHA256SUMS.txt"
    artifacts = [image, zsync, source_path]
    hashes.write_text("".join(f"{digest(path)}  {path.name}\n" for path in artifacts), encoding="utf-8")
    print(f"Packaged {image} ({image.stat().st_size / 1024 / 1024:.1f} MiB)")
    print(f"Update file: {zsync}")
    print(f"Source snapshot: {source_path}")
    print(f"Checksums: {hashes}")


if __name__ == "__main__":
    main()
