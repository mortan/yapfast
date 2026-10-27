#!/usr/bin/env python3
"""Package the macOS executable as Yapfast.app, with its icon, Info.plist and
the dependencies' licenses and notices, signed, and zipped for the release
beside the source snapshot.

scripts/build-macos runs it in scripts/macos-env, after the release build."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import urllib.parse

from release_package import ROOT, dependency_notices, digest, license_file, names_home, source_snapshot

TARGET = "aarch64-apple-darwin"
FLAVOURS = {
    "discord": {"features": ["--no-default-features", "--features", "desktop,discord"], "suffix": ""},
    "full": {"features": [], "suffix": "-full-preview"},
}
CACHE = Path.home() / "Library/Caches/yapfast-build"
# The third-party code in rust-skia's prebuilt macOS Skia, as its archive's
# object files name it; nix/flake.nix pins each one's source.
SKIA_DEPENDENCIES = ("expat", "libjpeg-turbo", "libpng", "wuffs", "zlib")
WEBRTC_ARCHIVE_SHA256 = "f9be49b9fee9cd1588da8a40c85bda21f5ea49c4ed82662dc889b1c919569a08"


def nar_hash(path):
    """The SRI hash Nix gives the folder at path, which nix/flake.nix pins
    each fetchzip by."""
    hasher = hashlib.sha256()

    def string(value):
        value = value.encode() if isinstance(value, str) else value
        hasher.update(len(value).to_bytes(8, "little") + value + b"\0" * (-len(value) % 8))

    def node(item):
        string("(")
        mode = item.lstat().st_mode
        if stat.S_ISLNK(mode):
            for part in ("type", "symlink", "target", os.readlink(item)):
                string(part)
        elif stat.S_ISREG(mode):
            string("type")
            string("regular")
            if mode & stat.S_IXUSR:
                string("executable")
                string("")
            string("contents")
            string(item.read_bytes())
        else:
            string("type")
            string("directory")
            for child in sorted(item.iterdir(), key=lambda child: os.fsencode(child.name)):
                for part in ("entry", "(", "name", child.name, "node"):
                    string(part)
                node(child)
                string(")")
        string(")")

    string("nix-archive-1")
    node(path)
    return "sha256-" + base64.b64encode(hasher.digest()).decode()


def fetch(url, destination):
    subprocess.run(["curl", "--fail", "--location", "--silent", "--show-error", "--output", str(destination), url],
                   check=True, timeout=1800)


def skia_source():
    """Skia's source with the third-party code the macOS archive holds, the
    revisions nix/flake.nix pins, for their notices: the prebuilt archive
    carries only Skia's own license. Downloaded once into CACHE, where only the
    notices stay; its presence means it's complete."""
    flake = (ROOT / "nix/flake.nix").read_text()
    skia = re.search(r'windowsSkiaSourceArchive = pkgs\.fetchurl \{\s*url = "([^"]+)";\s*sha256 = "([0-9a-f]{64})";',
                     flake)
    pins = {name: (url, sri) for name, url, sri in
            re.findall(r'\(skiaDependency "([^"]+)" "([^"]+)" "(sha256-[^"]+)"\)', flake)}
    if not skia or not all(name in pins for name in SKIA_DEPENDENCIES):
        raise SystemExit("nix/flake.nix no longer pins Skia's source as scripts/package-macos.py reads it")
    url, sha256 = skia.groups()
    source = CACHE / f"skia-source-{url.rsplit('/', 1)[1][:8]}"
    if source.is_dir():
        return source
    CACHE.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="skia-source.", dir=CACHE) as temporary:
        temporary = Path(temporary)
        archive = temporary / "skia.tar.gz"
        print(f"Downloading Skia's source for its notices: {url}")
        fetch(url, archive)
        if digest(archive) != sha256:
            raise SystemExit(f"{url} doesn't match its pinned SHA-256")
        unpacked = temporary / "skia"
        with tarfile.open(archive) as stream:
            stream.extractall(unpacked, filter="data")
        archive.unlink()
        [tree] = unpacked.iterdir()
        for name in SKIA_DEPENDENCIES:
            url, sri = pins[name]
            print(f"Downloading {name}: {url}")
            archive = temporary / f"{name}.tar.gz"
            fetch(url, archive)
            destination = tree / "third_party/externals" / name
            destination.mkdir(parents=True)
            with tarfile.open(archive) as stream:
                stream.extractall(destination, filter="data")
            archive.unlink()
            if nar_hash(destination) != sri:
                raise SystemExit(f"{url} doesn't match the hash nix/flake.nix pins")
        # Only the notices stay: 350 MiB of source, 3 of them.
        for path in sorted(tree.rglob("*"), reverse=True):
            if path.is_symlink() or (path.is_file() and not license_file(path)):
                path.unlink()
            elif path.is_dir() and not any(path.iterdir()):
                path.rmdir()
        tree.rename(source)
    return source


def sign(app, identity):
    # Without the hardened runtime, which only notarization needs, the app
    # needs no entitlements for the microphone, camera or screen.
    subprocess.run(["codesign", "--force", "--sign", identity, "--timestamp=none", str(app)],
                   check=True, timeout=300)
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True, timeout=300)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--executable", type=Path, default=ROOT / "target" / TARGET / "release/yapfast")
    parser.add_argument("--flavour", choices=FLAVOURS, default="discord",
                        help="the features the executable was built with (default: discord)")
    parser.add_argument("--output", type=Path, default=ROOT / "target/dist",
                        help="where Yapfast.app goes; its ZIP goes to OUTPUT/macos (default: target/dist)")
    args = parser.parse_args()
    flavour = FLAVOURS[args.flavour]
    executable = args.executable.resolve()
    newest_input = max(path.stat().st_mtime for path in
                       [ROOT / "Cargo.toml", ROOT / "Cargo.lock", ROOT / "build.rs",
                        *(ROOT / "src").rglob("*.rs"), *(ROOT / "ui").rglob("*")]
                       if path.is_file())
    if executable.stat().st_mtime < newest_input:
        raise SystemExit("The executable predates the source; run scripts/build-macos first")
    archs = subprocess.check_output(["lipo", "-archs", str(executable)], text=True, timeout=30).split()
    if archs != ["arm64"]:
        raise SystemExit(f"Expected an arm64 executable, not {' '.join(archs)}")
    contents = executable.read_bytes()
    if names_home(contents):
        raise SystemExit(f"{executable} names {Path.home()}; build it with scripts/build-macos")
    # Both flavours build to the same path; the Matrix SDK's source paths
    # tell which one is there.
    if (b"/matrix-rust-sdk-" in contents) != (args.flavour == "full"):
        raise SystemExit(f"{executable} isn't the {args.flavour} flavour; run scripts/build-macos {args.flavour}")
    del contents
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", TARGET,
         *flavour["features"]],
        cwd=ROOT, timeout=30,
    ))
    version = next(package["version"] for package in metadata["packages"] if package["name"] == "yapfast")
    skia_url = urllib.parse.urlparse(os.environ["SKIA_BINARIES_URL"])
    if skia_url.scheme != "file":
        raise SystemExit("Packaging requires the pinned local Skia archive")
    skia_archive = Path(urllib.parse.unquote(skia_url.path))
    skia = skia_source()
    identity = os.environ.get("YAPFAST_CODESIGN_IDENTITY") or "-"

    releases = args.output / "macos"
    releases.mkdir(parents=True, exist_ok=True)
    base = f"Yapfast-{version}{flavour['suffix']}"
    archive_path = releases / f"{base}-macos-arm64.zip"
    source_path = releases / f"{base}-source.zip"
    app_path = args.output / "Yapfast.app"
    with tempfile.TemporaryDirectory(prefix="yapfast-macos-package-") as temporary:
        temporary = Path(temporary)
        rtc = source_snapshot(metadata, base, temporary, source_path)

        app = temporary / "Yapfast.app"
        (app / "Contents/MacOS").mkdir(parents=True)
        resources = app / "Contents/Resources"
        resources.mkdir()
        # Info.plist's versions take only numbers, without a nightly's suffix.
        info = (ROOT / "packaging/macos/Info.plist").read_text(encoding="utf-8")
        (app / "Contents/Info.plist").write_text(info.replace("@VERSION@", version.split("-")[0]), encoding="utf-8")
        shutil.copy2(executable, app / "Contents/MacOS/yapfast")
        shutil.copy2(ROOT / "packaging/macos/yapfast.icns", resources / "yapfast.icns")
        shutil.copy2(ROOT / "LICENSE", resources / "LICENSE")
        source_url = f"https://github.com/yapfast/yapfast/releases/tag/v{version}{flavour['suffix']}"
        (resources / "SOURCE.txt").write_text(
            f"The source code of this build of Yapfast {version}"
            + (", with the exact source of\nthe libraries it includes under the AGPL," if rtc else "")
            + " is published with it:\n\n"
            f"  {source_url}\n  {source_path.name}, SHA-256 in BUILD.json\n\n"
            "and in the repository, https://github.com/yapfast/yapfast. The licenses folder\n"
            "holds every dependency's own license and notices.\n",
            encoding="utf-8",
        )
        dependencies = dependency_notices(metadata, resources, resources / "licenses", skia_archive, skia,
                                          Path(os.environ["LK_CUSTOM_WEBRTC"]))
        (resources / "THIRD-PARTY.json").write_text(json.dumps(dependencies, indent=2) + "\n", encoding="utf-8")
        build = {
            "version": version, "flavour": args.flavour, "target": TARGET,
            "cargo_features": " ".join(flavour["features"]) or "default",
            "rustc": subprocess.check_output(["rustc", "--version"], text=True, timeout=10).strip(),
            "macos_sdk": subprocess.check_output(["xcrun", "--show-sdk-version"], text=True, timeout=10).strip(),
            "macos_deployment_target": os.environ["MACOSX_DEPLOYMENT_TARGET"],
            "ui_framework": "Slint 1.18.1",
            "skia_version": "0.153.3",
            "skia_archive_prepared_sha256": digest(skia_archive),
            "cargo_lock_sha256": digest(ROOT / "Cargo.lock"),
            "executable_sha256": digest(executable),
            "source_archive_sha256": digest(source_path),
            "webrtc_tag": "webrtc-51ef663",
            "webrtc_archive_sha256": WEBRTC_ARCHIVE_SHA256,
            "signature": "ad hoc" if identity == "-" else identity,
        }
        if rtc:
            build["homeserver_default"] = "https://matrix.org"
        (resources / "BUILD.json").write_text(json.dumps(build, indent=2) + "\n", encoding="utf-8")
        if app_path.exists():
            shutil.rmtree(app_path)
        shutil.move(app, app_path)
    sign(app_path, identity)
    archive_path.unlink(missing_ok=True)
    subprocess.run(["ditto", "-c", "-k", "--keepParent", str(app_path), str(archive_path)], check=True, timeout=600)
    hashes = releases / "SHA256SUMS.txt"
    hashes.write_text("".join(f"{digest(path)}  {path.name}\n" for path in (archive_path, source_path)),
                      encoding="utf-8")
    print(f"App: {app_path}")
    print(f"Packaged {archive_path} ({archive_path.stat().st_size / 1024 / 1024:.1f} MiB)")
    print(f"Source snapshot: {source_path}")
    print(f"Checksums: {hashes}")


if __name__ == "__main__":
    main()
