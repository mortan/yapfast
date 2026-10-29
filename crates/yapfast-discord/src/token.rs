//! The Discord token lives in the OS credential store, next to Yapfast's Matrix
//! session, and never in Matrix. So does the key [`crate::cache`] encrypts
//! its files with.

use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use anyhow::{Result, anyhow, bail};
use keyring::Entry;

const SERVICE: &str = "io.github.yapfast.Yapfast";
/// The linked Discord account, set by linking by QR code or by the developer
/// probe's store-token command.
const ACCOUNT: &str = "discord-token";
/// Where the first probe kept the token; moved to `ACCOUNT` when read.
const EARLIER_ACCOUNT: &str = "discord-probe-token";
/// The key of the files [`crate::cache`] keeps, in hex.
const CACHE_KEY: &str = "discord-cache-key";

fn entry() -> Result<Entry> {
    named(ACCOUNT)
}

fn named(account: &str) -> Result<Entry> {
    Entry::new(SERVICE, account).map_err(|_| anyhow!("could not access the OS credential store"))
}

/// With the test server, the link lasts while Yapfast runs. It starts linked,
/// with [`crate::client::TEST_TOKEN`], unless `YAPFAST_DISCORD_TEST_UNLINKED` is
/// set, which shows linking against the fake.
fn test_link() -> &'static Mutex<Option<String>> {
    static LINK: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    LINK.get_or_init(|| {
        let unlinked = std::env::var_os("YAPFAST_DISCORD_TEST_UNLINKED").is_some();
        Mutex::new((!unlinked).then(|| crate::client::TEST_TOKEN.to_owned()))
    })
}

/// No Discord account is linked on this device, which [`load`] says rather
/// than failing.
#[derive(Debug)]
pub struct NotLinked;

impl std::fmt::Display for NotLinked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no Discord account is linked")
    }
}

impl std::error::Error for NotLinked {}

pub fn store(token: &str) -> Result<()> {
    let token = clean(token)?;
    save(&token)?;
    known().set(Known::Token(token));
    Ok(())
}

fn save(token: &str) -> Result<()> {
    if crate::client::test_server().is_some() {
        *test_link()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(token.to_owned());
        return Ok(());
    }
    entry()?
        .set_password(token)
        .map_err(|_| anyhow!("could not save the token in the OS credential store"))
}

const UNREADABLE: &str = "could not read the token from the OS credential store";

/// What the credential store last said of the linked account. Linking and
/// unlinking keep it current, so asking again reads nothing: on macOS each
/// read of the Keychain may ask the person, and waits for the answer.
enum Known {
    Token(String),
    NotLinked,
    /// The store couldn't be read. `told` once [`load`] has said so; the
    /// load after, a retry, reads it again.
    Unreadable {
        told: bool,
    },
}

/// [`Known`], and how many times linking or unlinking changed it: a read
/// takes no lock while it waits, and its answer doesn't replace one that
/// came meanwhile.
#[derive(Default)]
struct Kept {
    known: Option<Known>,
    changes: u64,
}

impl Kept {
    fn set(&mut self, known: Known) {
        self.known = Some(known);
        self.changes += 1;
    }
}

static KNOWN: Mutex<Kept> = Mutex::new(Kept {
    known: None,
    changes: 0,
});

fn known() -> MutexGuard<'static, Kept> {
    lock(&KNOWN)
}

fn lock(kept: &Mutex<Kept>) -> MutexGuard<'_, Kept> {
    kept.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Known {
    fn of(read: &Result<String>, told: bool) -> Self {
        match read {
            Ok(token) => Self::Token(token.clone()),
            Err(error) if error.is::<NotLinked>() => Self::NotLinked,
            Err(_) => Self::Unreadable { told },
        }
    }

    fn linked(&self) -> Result<bool> {
        match self {
            Self::Token(_) => Ok(true),
            Self::NotLinked => Ok(false),
            Self::Unreadable { .. } => bail!(UNREADABLE),
        }
    }
}

/// Reads the store without holding `kept`, which others may ask meanwhile,
/// and keeps its answer unless linking or unlinking changed it first.
fn read_into(
    kept: &Mutex<Kept>,
    told: bool,
    read: impl FnOnce() -> Result<String>,
) -> Result<String> {
    let changes = lock(kept).changes;
    let read = read();
    let mut kept = lock(kept);
    if kept.changes == changes {
        kept.known = Some(Known::of(&read, told));
    }
    read
}

/// The linked account's token, read from the store unless it was read or
/// linked before. A store that couldn't be read is read again, but its
/// first failure, found by [`linked`], is answered without reading.
pub fn load() -> Result<String> {
    load_from(&KNOWN, read)
}

fn load_from(kept: &Mutex<Kept>, read: impl FnOnce() -> Result<String>) -> Result<String> {
    match &mut lock(kept).known {
        Some(Known::Token(token)) => return Ok(token.clone()),
        Some(Known::Unreadable { told }) if !*told => {
            *told = true;
            bail!(UNREADABLE);
        }
        _ => {}
    }
    read_into(kept, true, read)
}

/// Whether an account is linked on this device, as the store said the last
/// time it was read: only the first call reads it, which may wait for the
/// person on macOS. An error says it couldn't be read.
pub fn linked() -> Result<bool> {
    linked_from(&KNOWN, read)
}

fn linked_from(kept: &Mutex<Kept>, read: impl FnOnce() -> Result<String>) -> Result<bool> {
    if let Some(known) = &lock(kept).known {
        return known.linked();
    }
    read_into(kept, false, read)
        .map(|_| true)
        .or_else(|error| match error.is::<NotLinked>() {
            true => Ok(false),
            false => Err(error),
        })
}

/// What [`linked`] would say without reading the store: None until it, or
/// [`load`], has read it, or an account was linked or unlinked.
pub fn linked_known() -> Option<Result<bool>> {
    known().known.as_ref().map(Known::linked)
}

/// Reads the token from the store, moving one the first probe kept.
fn read() -> Result<String> {
    if crate::client::test_server().is_some() {
        return test_link()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| NotLinked.into());
    }
    match entry()?.get_password() {
        Ok(token) => return Ok(token),
        Err(keyring::Error::NoEntry) => {}
        Err(_) => bail!(UNREADABLE),
    }
    match named(EARLIER_ACCOUNT)?.get_password() {
        Ok(token) => {
            let token = clean(&token)?;
            save(&token)?;
            let _ = named(EARLIER_ACCOUNT)?.delete_credential();
            Ok(token)
        }
        Err(keyring::Error::NoEntry) => Err(NotLinked.into()),
        Err(_) => bail!(UNREADABLE),
    }
}

/// The key [`crate::cache`] encrypts its files with, made the first time.
/// With the test server, always the same one, so what a run kept shows in
/// the next, as with Discord.
pub(crate) fn cache_key() -> Result<crate::cache::Key> {
    if crate::client::test_server().is_some() {
        return Ok([7; 32]);
    }
    let entry = named(CACHE_KEY)?;
    match entry.get_password() {
        Ok(hex) => {
            if let Some(key) = crate::cache::read_key(&hex) {
                return Ok(key);
            }
        }
        Err(keyring::Error::NoEntry) => {}
        Err(_) => bail!("could not read the cache key from the OS credential store"),
    }
    // None yet, or one that isn't a key: what it encrypted can't be read.
    let key = crate::cache::new_key();
    entry
        .set_password(&crate::cache::write_key(&key))
        .map_err(|_| anyhow!("could not save the cache key in the OS credential store"))?;
    Ok(key)
}

pub fn forget() -> Result<()> {
    crate::cache::forget();
    // Until it's gone, whether it is is read again.
    {
        let mut kept = known();
        kept.known = None;
        kept.changes += 1;
    }
    if crate::client::test_server().is_some() {
        *test_link()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    } else {
        for account in [ACCOUNT, EARLIER_ACCOUNT, CACHE_KEY] {
            match named(account)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(_) => bail!("could not remove the token from the OS credential store"),
            }
        }
    }
    known().set(Known::NotLinked);
    Ok(())
}

/// Accept the header value as copied from browser developer tools, with or
/// without surrounding quotes or whitespace.
pub fn clean(token: &str) -> Result<String> {
    let token = token.trim().trim_matches(['"', '\'']).trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        bail!("That doesn't look like a Discord token.");
    }
    Ok(token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_store_once_and_again_only_to_retry_a_failed_read() {
        let reads = std::cell::Cell::new(0);
        let failing = || -> Result<String> {
            reads.set(reads.get() + 1);
            bail!(UNREADABLE)
        };
        let known = Mutex::default();
        assert!(linked_from(&known, failing).is_err());
        assert!(linked_from(&known, failing).is_err());
        assert_eq!(reads.get(), 1, "the sign-in screen asks without reading");
        // Restoring then says why without asking the person a second time.
        assert!(load_from(&known, failing).is_err());
        assert_eq!(reads.get(), 1);
        // Retrying reads again.
        let working = || -> Result<String> {
            reads.set(reads.get() + 1);
            Ok("token".into())
        };
        assert_eq!(load_from(&known, working).unwrap(), "token");
        assert_eq!(reads.get(), 2);
        assert_eq!(load_from(&known, working).unwrap(), "token");
        assert!(linked_from(&known, working).unwrap());
        assert_eq!(reads.get(), 2, "the token read is kept");

        let known = Mutex::default();
        let unlinked = || -> Result<String> { Err(NotLinked.into()) };
        assert!(!linked_from(&known, unlinked).unwrap());
        assert!(!linked_from(&known, failing).unwrap(), "kept");
        assert!(load_from(&known, unlinked).unwrap_err().is::<NotLinked>());
    }

    /// A read, which may wait for the person, holds no lock, so the window
    /// can ask what's known meanwhile; and a link made meanwhile stays.
    #[test]
    fn reads_without_holding_the_lock() {
        let known = Mutex::default();
        let read = || -> Result<String> {
            assert!(known.try_lock().is_ok(), "the lock is free while reading");
            lock(&known).set(Known::Token("linked meanwhile".into()));
            Err(NotLinked.into())
        };
        assert!(load_from(&known, read).unwrap_err().is::<NotLinked>());
        assert_eq!(
            load_from(&known, || bail!("not read")).unwrap(),
            "linked meanwhile"
        );
        assert!(linked_from(&known, || bail!("not read")).unwrap());
    }

    #[test]
    fn cleans_copied_header_values() {
        assert_eq!(clean("  \"abc.def.ghi\"\n").unwrap(), "abc.def.ghi");
        assert!(clean("   ").is_err());
        assert!(clean("Bearer abc").is_err());
    }
}
