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
    *known() = Some(Known::Token(token));
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
/// read of the Keychain may ask the person.
enum Known {
    Token(String),
    NotLinked,
    /// The store couldn't be read. `told` once [`load`] has said so; the
    /// load after, a retry, reads it again.
    Unreadable {
        told: bool,
    },
}

fn known() -> MutexGuard<'static, Option<Known>> {
    static KNOWN: Mutex<Option<Known>> = Mutex::new(None);
    KNOWN.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Known {
    fn of(read: &Result<String>, told: bool) -> Self {
        match read {
            Ok(token) => Self::Token(token.clone()),
            Err(error) if error.is::<NotLinked>() => Self::NotLinked,
            Err(_) => Self::Unreadable { told },
        }
    }
}

/// The linked account's token, read from the store unless it was read or
/// linked before. A store that couldn't be read is read again, but its
/// first failure, found by [`linked`], is answered without reading.
pub fn load() -> Result<String> {
    load_from(&mut known(), read)
}

fn load_from(known: &mut Option<Known>, read: impl FnOnce() -> Result<String>) -> Result<String> {
    match known {
        Some(Known::Token(token)) => return Ok(token.clone()),
        Some(Known::Unreadable { told }) if !*told => {
            *told = true;
            bail!(UNREADABLE);
        }
        _ => {}
    }
    let read = read();
    *known = Some(Known::of(&read, true));
    read
}

/// Whether an account is linked on this device, as the store said the last
/// time it was read: only the first call reads it. An error says it
/// couldn't be read.
pub fn linked() -> Result<bool> {
    linked_from(&mut known(), read)
}

fn linked_from(known: &mut Option<Known>, read: impl FnOnce() -> Result<String>) -> Result<bool> {
    match known.get_or_insert_with(|| Known::of(&read(), false)) {
        Known::Token(_) => Ok(true),
        Known::NotLinked => Ok(false),
        Known::Unreadable { .. } => bail!(UNREADABLE),
    }
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
    *known() = None;
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
    *known() = Some(Known::NotLinked);
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
        let mut known = None;
        assert!(linked_from(&mut known, failing).is_err());
        assert!(linked_from(&mut known, failing).is_err());
        assert_eq!(reads.get(), 1, "the sign-in screen asks without reading");
        // Restoring then says why without asking the person a second time.
        assert!(load_from(&mut known, failing).is_err());
        assert_eq!(reads.get(), 1);
        // Retrying reads again.
        let working = || -> Result<String> {
            reads.set(reads.get() + 1);
            Ok("token".into())
        };
        assert_eq!(load_from(&mut known, working).unwrap(), "token");
        assert_eq!(reads.get(), 2);
        assert_eq!(load_from(&mut known, working).unwrap(), "token");
        assert!(linked_from(&mut known, working).unwrap());
        assert_eq!(reads.get(), 2, "the token read is kept");

        let mut known = None;
        let unlinked = || -> Result<String> { Err(NotLinked.into()) };
        assert!(!linked_from(&mut known, unlinked).unwrap());
        assert!(!linked_from(&mut known, failing).unwrap(), "kept");
        assert!(
            load_from(&mut known, unlinked)
                .unwrap_err()
                .is::<NotLinked>()
        );
    }

    #[test]
    fn cleans_copied_header_values() {
        assert_eq!(clean("  \"abc.def.ghi\"\n").unwrap(), "abc.def.ghi");
        assert!(clean("   ").is_err());
        assert!(clean("Bearer abc").is_err());
    }
}
