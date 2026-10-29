#!/usr/bin/env python3
"""Package the macOS executable as Yapfast.app, with its icon, Info.plist and
the dependencies' licenses and notices, signed, and zipped with a README for
the release beside the source snapshot.

scripts/build-macos runs it in scripts/macos-env, after the build."""
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

from release_package import (FLAVOURS, ROOT, SKIA_VERSION, UI_FRAMEWORK, build_hashes, build_identity, cargo_metadata,
                             check_executable, dependency_notices, digest, first_steps, license_file,
                             local_skia_archive, source_notice, source_snapshot)

TARGET = "aarch64-apple-darwin"
CACHE = Path.home() / "Library/Caches/yapfast-build"
# scripts/make-macos-signing-identity makes it.
SIGNING_KEYCHAIN = Path.home() / "Library/Keychains/yapfast-signing.keychain-db"
# The third-party code in rust-skia's prebuilt macOS Skia, as its archive's
# object files name it; nix/flake.nix pins each one's source.
SKIA_DEPENDENCIES = ("expat", "libjpeg-turbo", "libpng", "wuffs", "zlib")


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
    notices stay; its presence means it's complete. Returns it with the URL
    and SHA-256 of Skia's own archive."""
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
        return source, url, sha256
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
    return source, url, sha256


def webrtc_pin():
    """The libwebrtc release tag and archive SHA-256 scripts/macos-env pins."""
    script = (ROOT / "scripts/macos-env").read_text()
    tag = re.search(r"^ *local webrtc_tag=(webrtc-[0-9a-f]+)$", script, re.MULTILINE)
    sha256 = re.search(r"^ *local webrtc_sha256=([0-9a-f]{64})$", script, re.MULTILINE)
    if not tag or not sha256:
        raise SystemExit("scripts/macos-env no longer pins libwebrtc as scripts/package-macos.py reads it")
    return tag[1], sha256[1]


def readme(flavour, minimum_system):
    """README.txt, beside the app in its ZIP."""
    steps, secrets = first_steps(flavour)
    edition = " (full preview)" if flavour == "full" else ""
    return (
        f"Yapfast{edition} for Macs with Apple silicon, macOS {minimum_system.removesuffix('.0')} or later\n\n"
        "1. Move Yapfast.app to Applications.\n"
        "2. Open it. It isn't notarized, so macOS stops it the first time: choose\n"
        "   Open Anyway in System Settings > Privacy & Security, or run\n"
        "   xattr -dr com.apple.quarantine /Applications/Yapfast.app in Terminal.\n"
        + steps + "\n"
        "macOS asks for the microphone, the camera, screen and system audio\n"
        "recording, input monitoring and notifications the first time Yapfast\n"
        "needs each; saying no stops only what needs it. Closing the window\n"
        "leaves Yapfast in the menu bar.\n"
        "Opened from Finder or the Dock, Yapfast writes diagnostics to\n"
        "~/Library/Logs/Yapfast, which Console shows. macOS stores session secrets\n"
        "in the login Keychain, and app data in\n"
        "~/Library/Application Support/io.github.yapfast.Yapfast. A new build may\n"
        "ask whether yapfast may use its Keychain items: choose Always Allow.\n\n"
        "Voice: select a voice channel and Join voice; the gear chooses devices.\n"
        "Use headphones for the first two-machine audio test.\n"
        "If anything fails, record the error text, macOS version, Mac model and\n"
        f"audio device model. Never send your {secrets} in logs.\n\n"
        "Yapfast draws with software rendering. Render with the GPU, in Settings >\n"
        "Advanced, switches to OpenGL from the next start; Yapfast goes back to\n"
        "software rendering if the GPU cannot start. To choose for one start, run\n"
        "/Applications/Yapfast.app/Contents/MacOS/yapfast --renderer software\n"
        "(or --renderer gpu) in Terminal.\n\n"
        "Yapfast is free software under the GNU General Public License, version 3\n"
        "or later, in Yapfast.app/Contents/Resources/LICENSE. Dependency notices\n"
        "and original licenses are beside it; SOURCE.txt there says where this\n"
        "build's source code is published.\n"
    )


def sign(app, identity):
    # Without the hardened runtime, which only notarization needs, the app
    # needs no entitlements for the microphone, camera or screen.
    keychain = []
    if identity == "Yapfast Dev" and SIGNING_KEYCHAIN.exists():
        # Its password is empty; it relocks when the user logs out.
        subprocess.run(["security", "unlock-keychain", "-p", "", str(SIGNING_KEYCHAIN)], check=True, timeout=30)
        keychain = ["--keychain", str(SIGNING_KEYCHAIN)]
    subprocess.run(["codesign", "--force", "--sign", identity, *keychain, "--timestamp=none", str(app)],
                   check=True, timeout=300)
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True, timeout=300)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--flavour", choices=FLAVOURS, default="discord",
                        help="the features the executable was built with (default: discord)")
    parser.add_argument("--profile", default="release",
                        help="the Cargo profile it was built with; any but release makes a dev package")
    parser.add_argument("--executable", type=Path, help="default: target/aarch64-apple-darwin/PROFILE/yapfast")
    parser.add_argument("--output", type=Path, default=ROOT / "target/dist",
                        help="where Yapfast.app goes; its ZIP goes to OUTPUT/macos/FLAVOUR, or FLAVOUR-dev "
                             "(default: target/dist)")
    args = parser.parse_args()
    flavour = FLAVOURS[args.flavour]
    executable = (args.executable or ROOT / "target" / TARGET / args.profile / "yapfast").resolve()
    check_executable(executable, args.flavour, "scripts/build-macos")
    archs = subprocess.check_output(["lipo", "-archs", str(executable)], text=True, timeout=30).split()
    if archs != ["arm64"]:
        raise SystemExit(f"Expected an arm64 executable, not {' '.join(archs)}")
    metadata, version = cargo_metadata(TARGET, flavour["features"])
    skia_archive = local_skia_archive(os.environ["SKIA_BINARIES_URL"])
    # nix/prepare-skia-native.py's record of the prebuilt archive it prepared.
    skia_binaries = json.loads((skia_archive.parent / "yapfast-skia-native.json").read_text())
    skia, skia_source_url, skia_source_sha256 = skia_source()
    webrtc_tag, webrtc_sha256 = webrtc_pin()
    minimum_system = os.environ["MACOSX_DEPLOYMENT_TARGET"]
    identity = os.environ.get("YAPFAST_CODESIGN_IDENTITY") or ("Yapfast Dev" if SIGNING_KEYCHAIN.exists() else "-")

    # A dev package, from an incremental build, gets a name and folder of its
    # own, so that it never takes a release's place.
    dev = "" if args.profile == "release" else "-dev"
    releases = args.output / "macos" / f"{args.flavour}{dev}"
    releases.mkdir(parents=True, exist_ok=True)
    base = f"Yapfast-{version}{flavour['suffix']}{dev}"
    archive_path = releases / f"{base}-macos-arm64.zip"
    source_path = releases / f"{base}-source.zip"
    app_path = args.output / "Yapfast.app"
    with tempfile.TemporaryDirectory(prefix="yapfast-macos-package-") as temporary:
        temporary = Path(temporary)
        rtc = source_snapshot(metadata, base, temporary, source_path)

        # The ZIP's folder: the app, and README.txt beside it.
        package = temporary / f"{base}-macos-arm64"
        app = package / "Yapfast.app"
        (app / "Contents/MacOS").mkdir(parents=True)
        resources = app / "Contents/Resources"
        resources.mkdir()
        # Info.plist's versions take only numbers, without a nightly's suffix.
        info = (ROOT / "packaging/macos/Info.plist").read_text(encoding="utf-8")
        info = info.replace("@VERSION@", version.split("-")[0]).replace("@MINIMUM_SYSTEM@", minimum_system)
        (app / "Contents/Info.plist").write_text(info, encoding="utf-8")
        shutil.copy2(executable, app / "Contents/MacOS/yapfast")
        shutil.copy2(ROOT / "packaging/macos/yapfast.icns", resources / "yapfast.icns")
        shutil.copy2(ROOT / "LICENSE", resources / "LICENSE")
        (resources / "SOURCE.txt").write_text(source_notice(version, flavour["suffix"], source_path, rtc),
                                              encoding="utf-8")
        dependencies = dependency_notices(metadata, resources, resources / "licenses", skia_archive, skia,
                                          Path(os.environ["LK_CUSTOM_WEBRTC"]))
        (resources / "THIRD-PARTY.json").write_text(json.dumps(dependencies, indent=2) + "\n", encoding="utf-8")
        build = {
            **build_identity(version, args.flavour, TARGET, flavour["features"]),
            "cargo_profile": args.profile,
            "rustc": subprocess.check_output(["rustc", "--version"], text=True, timeout=10).strip(),
            "macos_sdk": subprocess.check_output(["xcrun", "--show-sdk-version"], text=True, timeout=10).strip(),
            "macos_deployment_target": minimum_system,
            "ui_framework": UI_FRAMEWORK,
            "renderer_default": "skia-software",
            "renderer_gpu": "skia-opengl, falling back to skia-software",
            "skia_version": SKIA_VERSION,
            "skia_source_revision": skia_source_url.rsplit("/", 1)[1],
            "skia_source_archive_sha256": skia_source_sha256,
            "skia_binaries": skia_binaries["key"],
            "skia_archive_sha256": skia_binaries["upstream_sha256"],
            **build_hashes(skia_archive, executable, source_path),
            "webrtc_tag": webrtc_tag,
            "webrtc_archive_sha256": webrtc_sha256,
            "signature": "ad hoc" if identity == "-" else identity,
        }
        if rtc:
            build["homeserver_default"] = "https://matrix.org"
        (resources / "BUILD.json").write_text(json.dumps(build, indent=2) + "\n", encoding="utf-8")
        (package / "README.txt").write_text(readme(args.flavour, minimum_system), encoding="utf-8")
        sign(app, identity)
        archive_path.unlink(missing_ok=True)
        subprocess.run(["ditto", "-c", "-k", "--keepParent", str(package), str(archive_path)], check=True,
                       timeout=600)
        if app_path.exists():
            shutil.rmtree(app_path)
        shutil.move(app, app_path)
    hashes = releases / "SHA256SUMS.txt"
    hashes.write_text("".join(f"{digest(path)}  {path.name}\n" for path in (archive_path, source_path)),
                      encoding="utf-8")
    print(f"App: {app_path}")
    print(f"Packaged {archive_path} ({archive_path.stat().st_size / 1024 / 1024:.1f} MiB)")
    print(f"Source snapshot: {source_path}")
    print(f"Checksums: {hashes}")


if __name__ == "__main__":
    main()
