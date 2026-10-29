//! The games Discord detects, from its /games/detectable, kept on disk as a
//! compact list of this platform's executables and Steam ids. Memory holds
//! only a hash of each executable's file name: a process whose name has one
//! has the list read again for that name's rules, which are kept after.

use crate::api::{Api, GameList};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    fs::File,
    hash::{Hash, Hasher},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

const MAX_FILE: u64 = 32 * 1024 * 1024;
/// How often the list is asked for again. An unchanged list costs only an
/// answer that it's the same.
pub(super) const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The most file names whose rules are kept. Few running programs have a
/// name the list knows; past this many, the kept rules start over.
const KEPT_NAMES: usize = 256;

#[derive(Debug, Deserialize, Serialize)]
struct Game {
    #[serde(default)]
    id: Option<String>,
    name: Box<str>,
    #[serde(default)]
    executables: Vec<Executable>,
    #[serde(default)]
    third_party_skus: Vec<Sku>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Executable {
    name: String,
    os: String,
    #[serde(default)]
    is_launcher: bool,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Sku {
    distributor: String,
    id: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct GameInfo {
    pub(super) id: Option<u64>,
    pub(super) name: Box<str>,
}

impl GameInfo {
    fn of(game: &Game) -> Arc<Self> {
        Arc::new(Self {
            id: game.id.as_deref().and_then(|id| id.parse().ok()),
            name: game.name.clone(),
        })
    }
}

struct Rule {
    suffix: Box<str>,
    windows: bool,
    arguments: Option<Box<str>>,
    game: Arc<GameInfo>,
}

impl Executable {
    /// The path the rule matches the end of, and its lowercase file name.
    fn rule(&self) -> Option<(String, String)> {
        let windows = self.os == "win32";
        let suffix = normalize(
            self.name.trim_start_matches('>'),
            windows || self.os == "darwin",
        );
        let file = suffix.rsplit('/').next().filter(|s| !s.is_empty())?;
        let file = file.to_lowercase();
        Some((suffix, file))
    }
}

/// What a read of the list found: rules by file name hash, and games by
/// Steam id.
#[derive(Default)]
struct Found {
    rules: HashMap<u64, Vec<Rule>>,
    steam: HashMap<u64, Arc<GameInfo>>,
}

#[derive(Default)]
pub(super) struct Catalog {
    /// The compact list, read again for the names a scan finds.
    file: Option<File>,
    /// Hashes of the lowercase file names the list has rules for, and of
    /// those whose rules need the command line; both sorted.
    names: Vec<u64>,
    with_arguments: Vec<u64>,
    rules: usize,
    /// The rules read for a file name, by its hash: none if it has none.
    kept: RefCell<HashMap<u64, Box<[Rule]>>>,
}

/// Whether Windows games run here through Wine, which they match as on
/// Windows: on Linux, and on macOS under CrossOver, Whisky or the Game
/// Porting Toolkit.
pub(super) const WINE: bool = cfg!(any(target_os = "linux", target_os = "macos"));

pub(super) fn host_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

fn name_hash(file: &str) -> u64 {
    let mut hash = DefaultHasher::new();
    file.hash(&mut hash);
    hash.finish()
}

fn read_games(reader: impl Read, visit: impl FnMut(Game) -> Result<()>) -> Result<usize> {
    // Filter each entry as it is read. Keeping all platforms/SKUs until the
    // complete 13+ MiB response is parsed needlessly doubles peak allocations.
    struct Games<F>(F);
    impl<'de, F: FnMut(Game) -> Result<()>> serde::de::Visitor<'de> for Games<F> {
        type Value = usize;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a game catalogue array")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            mut self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut count = 0;
            while let Some(mut game) = seq.next_element::<Game>()? {
                game.executables.retain(|exe| {
                    !exe.is_launcher
                        && !exe.name.is_empty()
                        && (exe.os == host_os() || (WINE && exe.os == "win32"))
                });
                game.third_party_skus
                    .retain(|sku| sku.distributor == "steam" && sku.id.is_some());
                if !game.name.trim().is_empty()
                    && game.name.chars().count() <= 128
                    && game
                        .id
                        .as_deref()
                        .is_none_or(|id| id.parse::<u64>().is_ok_and(|n| n > 0))
                    && (!game.executables.is_empty() || !game.third_party_skus.is_empty())
                {
                    (self.0)(game).map_err(serde::de::Error::custom)?;
                    count += 1;
                }
            }
            Ok(count)
        }
    }
    use serde::Deserializer;
    let mut decoder = serde_json::Deserializer::from_reader(BufReader::new(reader));
    let count = (&mut decoder).deserialize_seq(Games(visit))?;
    decoder.end()?;
    Ok(count)
}

/// Opens a list, refusing one too big to be Discord's.
fn open(path: &Path) -> Result<File> {
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.len() <= MAX_FILE,
        "game catalogue exceeds 32 MiB"
    );
    Ok(file)
}

impl Catalog {
    pub(super) fn read(path: &Path) -> Result<Self> {
        let file = open(path)?;
        let mut catalog = Self::default();
        read_games(&file, |game| {
            catalog.learn(&game);
            Ok(())
        })?;
        catalog.finish(file);
        Ok(catalog)
    }

    fn learn(&mut self, game: &Game) {
        for exe in &game.executables {
            let Some((_, file)) = exe.rule() else {
                continue;
            };
            let hash = name_hash(&file);
            self.names.push(hash);
            if exe.arguments.is_some() {
                self.with_arguments.push(hash);
            }
            self.rules += 1;
        }
    }

    fn finish(&mut self, file: File) {
        for hashes in [&mut self.names, &mut self.with_arguments] {
            hashes.sort_unstable();
            hashes.dedup();
            hashes.shrink_to_fit();
        }
        self.file = Some(file);
    }

    /// Reads the list once, for the rules of these file names and the games
    /// of these Steam ids.
    fn look_up(&self, names: &HashSet<u64>, steam: &HashSet<u64>) -> Result<Found> {
        let mut found = Found::default();
        let Some(mut file) = self.file.as_ref() else {
            return Ok(found);
        };
        file.seek(SeekFrom::Start(0))?;
        read_games(file, |game| {
            let mut info = None;
            for sku in &game.third_party_skus {
                if let Some(id) = sku.id.as_deref().and_then(|id| id.parse().ok())
                    && steam.contains(&id)
                {
                    let info = info.get_or_insert_with(|| GameInfo::of(&game));
                    found.steam.entry(id).or_insert_with(|| info.clone());
                }
            }
            for exe in &game.executables {
                let Some((suffix, file)) = exe.rule() else {
                    continue;
                };
                let hash = name_hash(&file);
                if names.contains(&hash) {
                    found.rules.entry(hash).or_default().push(Rule {
                        suffix: suffix.into_boxed_str(),
                        windows: exe.os == "win32",
                        arguments: exe.arguments.as_deref().map(Into::into),
                        game: info.get_or_insert_with(|| GameInfo::of(&game)).clone(),
                    });
                }
            }
            Ok(())
        })?;
        Ok(found)
    }

    /// Has the rules for the file names of these paths read, all at once,
    /// so matching them doesn't read the list once for each.
    pub(super) fn prepare<'a>(&self, paths: impl IntoIterator<Item = (&'a str, bool)>) {
        let kept = self.kept.borrow();
        let wanted: HashSet<u64> = paths
            .into_iter()
            .flat_map(|(path, windows)| file_names(path, windows))
            .map(|file| name_hash(&file))
            .filter(|hash| self.names.binary_search(hash).is_ok() && !kept.contains_key(hash))
            .collect();
        drop(kept);
        if !wanted.is_empty() {
            self.keep(wanted);
        }
    }

    fn keep(&self, wanted: HashSet<u64>) {
        let mut rules = match self.look_up(&wanted, &HashSet::new()) {
            Ok(found) => found.rules,
            Err(error) => {
                tracing::debug!(%error, "could not read the game catalogue again");
                HashMap::new()
            }
        };
        let mut kept = self.kept.borrow_mut();
        if kept.len() + wanted.len() > KEPT_NAMES {
            kept.clear();
        }
        for hash in wanted {
            let found = rules.remove(&hash).unwrap_or_default();
            kept.insert(hash, found.into_boxed_slice());
        }
    }

    /// The games of these Steam ids, from one read of the list.
    pub(super) fn steam_games(&self, ids: &HashSet<u64>) -> HashMap<u64, Arc<GameInfo>> {
        if ids.is_empty() {
            return HashMap::new();
        }
        match self.look_up(&HashSet::new(), ids) {
            Ok(found) => found.steam,
            Err(error) => {
                tracing::debug!(%error, "could not read the game catalogue for Steam");
                HashMap::new()
            }
        }
    }

    pub(super) fn matches(
        &self,
        path: &str,
        args: &[std::ffi::OsString],
        windows: bool,
    ) -> Option<Arc<GameInfo>> {
        if cfg!(target_os = "macos") && !windows {
            return self.matches_macos(path, args);
        }
        let path = normalize(path, windows);
        self.match_path(&path, args, windows)
    }

    fn matches_macos(&self, path: &str, args: &[std::ffi::OsString]) -> Option<Arc<GameInfo>> {
        let path = normalize(path, true);
        self.match_path(&path, args, false)
            .or_else(|| self.match_path(app_bundle(&path)?, args, false))
    }

    fn match_path(
        &self,
        path: &str,
        args: &[std::ffi::OsString],
        windows: bool,
    ) -> Option<Arc<GameInfo>> {
        let hash = name_hash(&path.rsplit('/').next()?.to_lowercase());
        self.names.binary_search(&hash).ok()?;
        if !self.kept.borrow().contains_key(&hash) {
            self.keep(HashSet::from([hash]));
        }
        self.kept
            .borrow()
            .get(&hash)?
            .iter()
            .find(|rule| {
                rule.windows == windows
                    && suffix_matches(path, &rule.suffix)
                    && rule
                        .arguments
                        .as_ref()
                        .is_none_or(|expected| arguments_match(args, expected))
            })
            .map(|rule| rule.game.clone())
    }

    /// Whether a rule for this path's file name, or on macOS its app
    /// bundle's, needs the command line.
    pub(super) fn needs_arguments(&self, path: &str) -> bool {
        file_names(path, false)
            .any(|file| self.with_arguments.binary_search(&name_hash(&file)).is_ok())
    }

    pub(super) fn rule_count(&self) -> usize {
        self.rules
    }
}

/// The app bundle a lowercase path is in, up to its `.app`. Discord, and
/// people, name a macOS app by its bundle, while sysinfo gives the
/// executable inside App.app/Contents/MacOS/.
pub(super) fn app_bundle(path: &str) -> Option<&str> {
    let end = path.find(".app/")? + 4;
    Some(&path[..end])
}

/// The lowercase file names a path may match by: on macOS, also its app
/// bundle's.
fn file_names(path: &str, windows: bool) -> impl Iterator<Item = String> {
    let path = normalize(path, windows || cfg!(target_os = "macos"));
    let bundle = (cfg!(target_os = "macos") && !windows)
        .then(|| app_bundle(&path)?.rsplit('/').next().map(str::to_lowercase))
        .flatten();
    path.rsplit('/')
        .next()
        .map(str::to_lowercase)
        .into_iter()
        .chain(bundle)
}

pub(super) fn normalize(path: &str, windows: bool) -> String {
    let mut path = path.replace('\\', "/");
    if windows {
        if let Some(rest) = path.strip_prefix("//?/UNC/") {
            path = format!("//{rest}");
        } else if path.starts_with("//?/") {
            path.drain(..4);
        }
        path.to_lowercase()
    } else {
        path
    }
}

fn suffix_matches(path: &str, suffix: &str) -> bool {
    path == suffix
        || path
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

fn arguments_match(args: &[std::ffi::OsString], expected: &str) -> bool {
    if args
        .iter()
        .any(|arg| arg.to_string_lossy().contains(expected))
    {
        return true;
    }
    let mut command = String::new();
    for arg in args {
        if !command.is_empty() {
            command.push(' ');
        }
        command.push_str(&arg.to_string_lossy());
    }
    command.contains(expected)
}

/// The first line of the file the CDN's version is kept in. A list kept
/// without it, by an earlier Yapfast, may hold other platforms' rules than
/// this one keeps (macOS kept no Windows rules before Wine), so it's
/// downloaded again at once.
const VERSION: &str = "2\n";

/// How long ago the list at `path` was fetched, or found unchanged; none
/// for a list an earlier Yapfast kept.
pub(super) fn age(path: &Path) -> Option<Duration> {
    let version = std::fs::read_to_string(etag_file(path)).ok()?;
    if !version.starts_with(VERSION) {
        return None;
    }
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// Where the CDN's version of the list at `cache` is kept.
fn etag_file(cache: &Path) -> PathBuf {
    cache.with_extension("etag")
}

/// What asking for the list again found.
pub(super) enum Refreshed {
    Unchanged,
    New(Catalog),
}

/// Downloads and compacts the list off the runtime, replacing the cache only
/// once the whole answer proved valid; a failure leaves the earlier list.
/// With `revalidate`, the cache holds the list the CDN's version kept
/// beside it names, and an unchanged list isn't downloaded again.
pub(super) async fn refresh(
    api: &Api,
    cache: Option<PathBuf>,
    revalidate: bool,
) -> Result<Refreshed> {
    let (temp, cache, etag) = tokio::task::spawn_blocking(move || -> Result<_> {
        let temp = match cache.as_ref().and_then(|p| p.parent()) {
            Some(parent) => {
                std::fs::create_dir_all(parent)?;
                tempfile::NamedTempFile::new_in(parent)?
            }
            None => tempfile::NamedTempFile::new()?,
        };
        let etag = cache
            .as_deref()
            .filter(|_| revalidate)
            .and_then(|cache| std::fs::read_to_string(etag_file(cache)).ok())
            .and_then(|etag| Some(etag.strip_prefix(VERSION)?.to_owned()))
            .filter(|etag| !etag.is_empty() && etag.len() <= 256);
        Ok((temp, cache, etag))
    })
    .await??;
    let answer = tokio::time::timeout(
        Duration::from_secs(30),
        api.detectable_games(temp.reopen()?, etag.as_deref()),
    )
    .await??;
    tokio::task::spawn_blocking(move || -> Result<_> {
        match answer {
            GameList::Unchanged => {
                // Its age says when to ask again.
                if let Some(cache) = &cache {
                    File::options()
                        .write(true)
                        .open(cache)?
                        .set_modified(SystemTime::now())?;
                }
                Ok(Refreshed::Unchanged)
            }
            GameList::Written(etag) => {
                let catalog = compact(temp.path(), cache.as_deref())?;
                if let Some(cache) = &cache {
                    let version = format!("{VERSION}{}", etag.unwrap_or_default());
                    if let Err(error) = std::fs::write(etag_file(cache), version) {
                        tracing::debug!(%error, "could not keep the game list's version");
                    }
                }
                Ok(Refreshed::New(catalog))
            }
        }
    })
    .await?
}

/// Writes the compact list from a download and learns its names, one game
/// at a time. The download stays readable until validation finishes, and a
/// bad response never replaces the cache. Without a cache file, the list
/// goes to a file that's gone once the catalogue is.
fn compact(source: &Path, cache: Option<&Path>) -> Result<Catalog> {
    let mut output = match cache {
        Some(path) => Output::Named(tempfile::NamedTempFile::new_in(
            path.parent().unwrap_or_else(|| Path::new(".")),
        )?),
        None => Output::Unnamed(tempfile::tempfile()?),
    };
    let mut writer = BufWriter::new(output.file());
    writer.write_all(b"[")?;
    let mut catalog = Catalog::default();
    let mut first = true;
    let count = read_games(open(source)?, |game| {
        if !first {
            writer.write_all(b",")?;
        }
        serde_json::to_writer(&mut writer, &game)?;
        first = false;
        catalog.learn(&game);
        Ok(())
    })
    .context("invalid game catalogue")?;
    ensure!(count > 0, "game catalogue contains no supported games");
    writer.write_all(b"]")?;
    writer.flush()?;
    drop(writer);
    let file = match (output, cache) {
        (Output::Named(file), Some(path)) => file.persist(path)?,
        (Output::Named(file), None) => file.into_file(),
        (Output::Unnamed(file), _) => file,
    };
    catalog.finish(file);
    Ok(catalog)
}

enum Output {
    Named(tempfile::NamedTempFile),
    Unnamed(File),
}

impl Output {
    fn file(&mut self) -> &mut File {
        match self {
            Self::Named(file) => file.as_file_mut(),
            Self::Unnamed(file) => file,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog(games: serde_json::Value) -> (tempfile::NamedTempFile, Catalog) {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), serde_json::to_vec(&games).unwrap()).unwrap();
        let catalog = Catalog::read(file.path()).unwrap();
        (file, catalog)
    }

    #[test]
    fn compact_cache_streams_supported_entries_and_preserves_old_cache_on_error() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("download.json");
        let cache = directory.path().join("cache.json");
        let game = json!({"id":"42","name":"Game","description":"discard this", "executables":[{"os":host_os(),"name":"game"}]});
        std::fs::write(&source, serde_json::to_vec(&[game]).unwrap()).unwrap();
        let indexed = compact(&source, Some(&cache)).unwrap();
        assert_eq!(indexed.rule_count(), 1);
        assert!(
            indexed
                .matches("/opt/game", &[], cfg!(target_os = "windows"))
                .is_some(),
            "the catalogue reads the cache it wrote"
        );
        assert_eq!(Catalog::read(&cache).unwrap().rule_count(), 1);
        let saved = std::fs::read(&cache).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        assert!(value[0].get("description").is_none());
        let mut invalid = std::fs::read(&source).unwrap();
        invalid.extend_from_slice(b" trailing invalid data");
        std::fs::write(&source, invalid).unwrap();
        assert!(compact(&source, Some(&cache)).is_err());
        assert_eq!(std::fs::read(&cache).unwrap(), saved);
        std::fs::write(&source, b"[]").unwrap();
        assert!(compact(&source, Some(&cache)).is_err());
        assert_eq!(std::fs::read(&cache).unwrap(), saved);
        // Without a cache, the list lives as long as the catalogue.
        std::fs::write(&source, &saved).unwrap();
        let unnamed = compact(&source, None).unwrap();
        std::fs::remove_file(&source).unwrap();
        assert!(
            unnamed
                .matches("/opt/game", &[], cfg!(target_os = "windows"))
                .is_some()
        );
    }

    #[test]
    fn reads_public_nullable_skus_and_discards_launchers_and_other_platforms() {
        let (file, catalog) = catalog(json!([
            {"id":"42", "name":"Verified Steam Game", "third_party_skus":[
                {"distributor":"battlenet", "id":null}, {"distributor":"steam", "id":"123"}],
                "executables":[{"name":"launcher", "os":host_os(), "is_launcher":true}]},
            {"id":"43", "name":"Game", "executables":[{"name":"native_game", "os":host_os()}]}
        ]));
        let steam = catalog.steam_games(&HashSet::from([123, 124]));
        assert_eq!(&*steam[&123].name, "Verified Steam Game");
        assert_eq!(steam[&123].id, Some(42));
        assert!(!steam.contains_key(&124));
        assert!(
            catalog
                .matches("/games/launcher", &[], cfg!(target_os = "windows"))
                .is_none()
        );
        assert!(
            catalog
                .matches("/games/native_game", &[], cfg!(target_os = "windows"))
                .is_some()
        );
        std::fs::write(file.path(), "[] trailing invalid data").unwrap();
        assert!(Catalog::read(file.path()).is_err());
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn keeps_only_name_hashes_and_reads_rules_once_per_name() {
        let (_file, catalog) = catalog(json!([
            {"id":"42","name":"Game","executables":[{"os":"win32","name":"game.exe"},{"os":host_os(),"name":"game"}]},
            {"id":"43","name":"Other","executables":[{"os":host_os(),"name":"other"}]}
        ]));
        assert!(
            catalog.kept.borrow().is_empty(),
            "no rules until a name is seen"
        );
        catalog.prepare([("/opt/game", false), ("/opt/unknown", false)]);
        assert_eq!(catalog.kept.borrow().len(), 1, "only names the list has");
        assert!(catalog.matches("/opt/game", &[], false).is_some());
        assert!(catalog.matches("C:/game.exe", &[], true).is_some());
        assert_eq!(catalog.kept.borrow().len(), 2, "game.exe and game differ");
        assert!(catalog.matches("/opt/other", &[], false).is_some());
        assert_eq!(catalog.kept.borrow().len(), 3);
    }

    // Linux and macOS keep Windows rules too, for Wine.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn matches_paths_on_component_boundaries_and_windows_case() {
        let (_file, catalog) = catalog(json!([{"id":"42","name":"Game","executables":[
            {"os":"win32","name":"Game/Game.EXE"},
            {"os":host_os(),"name":"game"}]}]));
        assert!(
            catalog
                .matches("C:\\Games\\GAME\\game.exe", &[], true)
                .is_some()
        );
        assert!(
            catalog
                .matches("C:/AnotherGame/game.exe", &[], true)
                .is_none()
        );
        assert!(catalog.matches("/opt/game", &[], false).is_some());
        assert!(catalog.matches("/opt/notgame", &[], false).is_none());
        assert!(catalog.match_path("/opt/GAME", &[], false).is_none());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_recognizes_the_app_bundle_instead_of_the_inner_executable() {
        let (_file, catalog) = catalog(json!([{
            "id":"42", "name":"Mac Game", "executables":[{"os":"darwin", "name":"game.app"}]
        }]));
        assert_eq!(
            &*catalog
                .matches_macos("/Applications/Game.app/Contents/MacOS/Game", &[])
                .unwrap()
                .name,
            "Mac Game"
        );
        assert!(
            catalog
                .matches_macos("/Applications/OtherGame.app/Contents/MacOS/Game", &[])
                .is_none()
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_reads_the_command_line_for_app_bundle_rules_with_arguments() {
        let (_file, catalog) = catalog(json!([{
            "name":"Runtime Game", "executables":[{"os":"darwin", "name":"runtime.app", "arguments":"--game fixture"}]
        }]));
        let path = "/Applications/Runtime.app/Contents/MacOS/Runtime";
        assert!(catalog.needs_arguments(path));
        assert!(!catalog.needs_arguments("/Applications/Other.app/Contents/MacOS/Runtime"));
        assert!(catalog.matches(path, &[], false).is_none());
        let args = [path, "--game", "fixture"].map(std::ffi::OsString::from);
        assert!(catalog.matches(path, &args, false).is_some());
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn windows_handles_verbatim_paths_and_rules_with_multiple_arguments() {
        assert_eq!(normalize(r"\\?\C:\Games\Fixture", true), "c:/games/fixture");
        assert_eq!(normalize(r"\\?\UNC\server\Games", true), "//server/games");
        let (_file, catalog) = catalog(json!([{
            "name":"Runtime Game", "executables":[{"os":"win32", "name":"java.exe", "arguments":"--game fixture"}]
        }]));
        assert!(catalog.needs_arguments("C:/Runtime/java.exe"));
        assert!(!catalog.needs_arguments("C:/Runtime/game.exe"));
        assert!(catalog.matches("C:/Runtime/java.exe", &[], true).is_none());
        let args = ["java.exe", "--game", "fixture"].map(std::ffi::OsString::from);
        assert!(
            catalog
                .matches("C:/Runtime/java.exe", &args, true)
                .is_some()
        );
    }
}
