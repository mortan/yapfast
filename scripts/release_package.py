"""What scripts/package-linux.py, scripts/package-windows.py and
scripts/package-macos.py share: the checks of the executable, the source
snapshot each release carries, its SOURCE.txt and the common part of its
BUILD.json, and the dependencies' licenses and notices each package holds."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import urllib.parse
import zipfile

ROOT = Path(__file__).resolve().parents[1]
# Cargo features and the suffix of file names and release tag, for each
# flavour scripts/build-windows and scripts/build-macos build. The Discord
# flavour is the release; the full one, with Matrix, goes out as a separate
# preview.
FLAVOURS = {
    "discord": {"features": ["--no-default-features", "--features", "desktop,discord"], "suffix": ""},
    "full": {"features": [], "suffix": "-full-preview"},
}
UI_FRAMEWORK = "Slint 1.18.1"
SKIA_VERSION = "0.153.3"
SOURCE_FILES = ("Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain.toml", "README.md", "LICENSE", ".gitignore")
SOURCE_DIRECTORIES = (".cargo", "src", "ui", "packaging", "crates", "scripts", "nix", "vendor", "testing", "docs")


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def zip_directory(source, destination):
    # Nix inputs use an epoch timestamp, which predates ZIP's 1980 minimum.
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=6, strict_timestamps=False) as archive:
        for path in sorted(source.rglob("*")):
            if path.is_file():
                archive.write(path, source.name + "/" + path.relative_to(source).as_posix())


def license_file(path):
    name = path.name.upper()
    # FTL.TXT is the FreeType License, which FreeType's LICENSE.TXT only names.
    return path.is_file() and (name.startswith(("LICENSE", "COPYING", "NOTICE", "OFL")) or name == "FTL.TXT")


def names_home(contents):
    """Whether a binary names this machine's home folder, as Rust's panic
    messages and C's asserts do when built without scripts/release-env."""
    return any(str(Path.home()).encode(encoding) in contents for encoding in ("utf-8", "utf-16-le"))


def dep_info_paths(text):
    """The files a Makefile rule from Cargo's dep-info depends on."""
    paths = []
    for line in text.splitlines():
        _, separator, dependencies = line.partition(": ")
        if separator:
            # Cargo escapes a space in a path with a backslash.
            paths += [path.replace("\\ ", " ") for path in re.split(r"(?<!\\) +", dependencies.strip()) if path]
    return paths


def newest_input(dep_info):
    """The newest modification time among an executable's inputs: the
    manifest and lockfile, whose version and dependencies go into it, and the
    files Cargo's dep-info names, the repository's sources and vendor/ among
    them. A folder a build script watches counts with the files in it, apart
    from dotfiles such as Finder's .DS_Store. A file that's gone counts as
    new."""
    newest = 0.0
    for name in ["Cargo.toml", "Cargo.lock", *dep_info_paths(dep_info.read_text(encoding="utf-8"))]:
        path = ROOT / name
        if path.is_dir():
            newest = max([newest, *(file.stat().st_mtime for file in path.rglob("*") if file.is_file()
                                    and not any(part.startswith(".") for part in file.relative_to(path).parts))])
        elif path.is_file():
            newest = max(newest, path.stat().st_mtime)
        else:
            return float("inf")
    return newest


def check_executable(executable, flavour, build_command):
    """Stops unless executable is a build of flavour from the current source,
    built as build_command, which names the flavour, builds it."""
    if not executable.is_file():
        raise SystemExit(f"There's no {executable}; run {build_command}")
    # Cargo writes the dep-info beside the executable it builds.
    dep_info = executable.with_suffix(".d")
    if not dep_info.is_file():
        raise SystemExit(f"There's no {dep_info}, which Cargo writes beside the executable; run {build_command}")
    if executable.stat().st_mtime < newest_input(dep_info):
        raise SystemExit(f"{executable} predates its source; run {build_command}")
    contents = executable.read_bytes()
    if names_home(contents):
        raise SystemExit(f"{executable} names {Path.home()}; build it with {build_command}")
    # Both flavours build to the same path; the Matrix SDK's source paths
    # tell which one is there.
    if (b"/matrix-rust-sdk-" in contents) != (flavour == "full"):
        raise SystemExit(f"{executable} isn't the {flavour} flavour; run {build_command}")


def skia_source_pin():
    """The URL and SHA-256 of the Skia source archive nix/flake.nix pins, the
    revision the prebuilt Skia archives are built from."""
    skia = re.search(r'windowsSkiaSourceArchive = pkgs\.fetchurl \{\s*url = "([^"]+)";\s*sha256 = "([0-9a-f]{64})";',
                     (ROOT / "nix/flake.nix").read_text())
    if not skia:
        raise SystemExit("nix/flake.nix no longer pins Skia's source as scripts/release_package.py reads it")
    return skia.groups()


def cargo_metadata(target, features):
    """Cargo's metadata of the packages these features build for target, and
    Yapfast's version."""
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target, *features],
        cwd=ROOT, timeout=30,
    ))
    return metadata, next(package["version"] for package in metadata["packages"] if package["name"] == "yapfast")


def local_skia_archive(location):
    """The prepared Skia archive that SKIA_BINARIES_URL, or location, names."""
    url = urllib.parse.urlparse(location)
    if url.scheme != "file":
        raise SystemExit("Packaging requires the pinned local Skia archive")
    return Path(urllib.parse.unquote(url.path))


def source_notice(version, suffix, source_path, rtc):
    """SOURCE.txt: where the source code of this build is published."""
    return (
        f"The source code of this build of Yapfast {version}"
        + (", with the exact source of\nthe libraries it includes under the AGPL," if rtc else "")
        + " is published with it:\n\n"
        f"  https://github.com/yapfast/yapfast/releases/tag/v{version}{suffix}\n"
        f"  {source_path.name}, SHA-256 in BUILD.json\n\n"
        "and in the repository, https://github.com/yapfast/yapfast. The licenses folder\n"
        "holds every dependency's own license and notices.\n"
    )


def first_steps(flavour):
    """README.txt's steps 3 and 4 for flavour, after it has started, and the
    secrets it holds that must never go in a report."""
    if flavour == "discord":
        return (
            "3. Link your Discord account: Yapfast shows a QR code, which Discord on\n"
            "   your phone scans (Settings, Scan QR Code). Discord doesn't allow apps\n"
            "   like Yapfast on personal accounts; linking puts your account at some\n"
            "   risk of being suspended.\n"
            "4. Open a channel, or a voice channel to talk.\n\n"
            "No account or saved session is included.\n"
        ), "password or Discord token"
    return (
        "3. Sign in with your Matrix account; the homeserver is editable.\n"
        "   Or link Discord, which shows a QR code for Discord on your phone.\n"
        "4. Open a room, or a voice channel to talk.\n\n"
        "Passwords, recovery keys and saved sessions are not included.\n"
    ), "password or recovery key"


def build_identity(version, flavour, target, features):
    """The fields that begin each BUILD.json."""
    return {"version": version, "flavour": flavour, "target": target,
            "cargo_features": " ".join(features) or "default"}


def build_hashes(skia_archive, executable, source_path):
    """The hashes BUILD.json records of what went into the package."""
    return {
        "skia_archive_prepared_sha256": digest(skia_archive),
        "cargo_lock_sha256": digest(ROOT / "Cargo.lock"),
        "executable_sha256": digest(executable),
        "source_archive_sha256": digest(source_path),
    }


def source_snapshot(metadata, base, temporary, destination):
    """Zips the repository's tracked source to destination, with the exact
    upstream RTC source the AGPL dependencies came from when the build has
    them. Returns whether it does."""
    source = temporary / f"{base}-source"
    source.mkdir()
    # Only what the repository tracks: the folders also hold private,
    # ignored material.
    tracked = subprocess.check_output(["git", "ls-files", "-z", "--", *SOURCE_FILES, *SOURCE_DIRECTORIES],
                                      cwd=ROOT, timeout=30).decode().split("\0")
    for name in filter(None, tracked):
        (source / name).parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / name, source / name)
    # Only the Matrix flavour builds the AGPL RTC dependencies.
    rtc = next((package for package in metadata["packages"] if package["name"] == "matrix-rtc-core"), None)
    if rtc:
        rtc_root = Path(rtc["manifest_path"]).parent.parent.parent
        shutil.copytree(rtc_root, source / "upstream/matrix-rust-rtc",
                        ignore=shutil.ignore_patterns(".git", "target", "node_modules"))
    # Every file takes the commit's time, so two builds of one commit, such as
    # a nightly's Linux and Windows jobs, zip the same snapshot.
    committed = int(subprocess.check_output(["git", "log", "-1", "--format=%ct"], cwd=ROOT, timeout=30))
    for path in source.rglob("*"):
        os.utime(path, (committed, committed), follow_symlinks=False)
    zip_directory(source, destination)
    return rtc is not None


def dependency_notices(metadata, package, licenses, skia_archive, skia_source, webrtc):
    """Copies each crate's, Skia's and libwebrtc's licenses and notices into
    licenses, and returns the crates' entries for THIRD-PARTY.json, with
    paths relative to package."""
    dependencies = []
    licenses.mkdir()
    for item in sorted(metadata["packages"], key=lambda value: (value["name"], value["version"])):
        if item["name"] == "yapfast":
            continue
        directory = Path(item["manifest_path"]).parent
        target = licenses / (item["name"] + "-" + item["version"])
        files = set()
        for parent in (directory, directory.parent, directory.parent.parent):
            files.update(path for path in parent.iterdir() if license_file(path))
        files.update(path for path in directory.rglob("*") if license_file(path))
        copied = []
        for index, path in enumerate(sorted(files)):
            target.mkdir(exist_ok=True)
            name = f"{index}-{path.name}"
            shutil.copy2(path, target / name)
            copied.append((target / name).relative_to(package).as_posix())
        dependencies.append({
            "name": item["name"], "version": item["version"], "license": item["license"],
            "source": item["source"], "repository": item["repository"], "license_files": copied,
        })
    with tarfile.open(skia_archive) as native:
        for member in native.getmembers():
            if member.isfile() and Path(member.name).name.upper().startswith(("LICENSE", "COPYING", "NOTICE")):
                with native.extractfile(member) as notice:
                    (licenses / ("skia-native-" + Path(member.name).name)).write_bytes(notice.read())
    for notice in skia_source.rglob("*"):
        if license_file(notice):
            destination = licenses / "skia-source" / notice.relative_to(skia_source)
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(notice, destination)
    shutil.copy2(webrtc / "LICENSE.md", licenses / "libwebrtc-LICENSE.md")
    # Some crates ship a license only their owner may read (fnv's are 0600),
    # and the AppImage's files all belong to root.
    for path in licenses.rglob("*"):
        if path.is_file():
            path.chmod(0o644)
    return dependencies
