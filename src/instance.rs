//! One Yapfast for each data folder, unless Settings → Advanced allows more:
//! starting it again brings the running one's window forward and ends. The
//! first start holds `instance.lock` in the folder, which the system lets go
//! of when the process ends, and listens for later starts: on a Unix socket
//! beside the lock, or where that path is too long for one in a short folder
//! of the user's, which carries the desktop's activation token, or on Windows
//! a named event. See docs/tray.md.

use std::{fs::File, path::Path, time::Duration};

const LOCK: &str = "instance.lock";
#[cfg(unix)]
const SOCKET: &str = "instance.socket";

/// How long a later start keeps trying to reach the first, which may still
/// be starting, or restarting with another renderer.
const PATIENCE: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_millis(100);

/// What this start of Yapfast goes on to do.
pub enum Start {
    /// Run, as the first on the folder or one more beside it.
    Run(Instance),
    /// Another Yapfast runs on the folder and was asked to show its window,
    /// unless this start was for the tray only or it didn't answer: nothing
    /// is left to do.
    Handed,
}

/// A running Yapfast's hold on its folder, while it's the first there.
#[derive(Default)]
pub struct Instance {
    _lock: Option<File>,
    listener: Option<Listener>,
}

/// Starts Yapfast on `root`. With another running there and `alone`, hands
/// this start to it, which shows its window unless `background`.
pub fn claim(root: &Path, alone: bool, background: bool) -> Start {
    claim_with(root, alone, background, activation_token())
}

fn claim_with(root: &Path, alone: bool, background: bool, token: Option<String>) -> Start {
    let lock = std::fs::create_dir_all(root).and_then(|()| {
        File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join(LOCK))
    });
    let lock = match lock {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(%error, "could not tell whether Yapfast already runs");
            return Start::Run(Instance::default());
        }
    };
    match lock.try_lock() {
        Ok(()) => {
            let listener = Listener::bind(root);
            Start::Run(Instance {
                _lock: Some(lock),
                listener,
            })
        }
        Err(std::fs::TryLockError::WouldBlock) if !alone => {
            tracing::info!("another Yapfast runs on this folder; starting one more");
            Start::Run(Instance::default())
        }
        Err(std::fs::TryLockError::WouldBlock) if background => {
            tracing::info!("Yapfast already runs; nothing to start in the tray");
            Start::Handed
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            if hand(root, token.as_deref()) {
                tracing::info!("Yapfast already runs; showed its window");
                Start::Handed
            } else {
                // A second Yapfast on the folder, unasked, would write the
                // same files as the first.
                tracing::warn!("Yapfast already runs on this folder but didn't answer; ending");
                Start::Handed
            }
        }
        Err(std::fs::TryLockError::Error(error)) => {
            tracing::warn!(%error, "could not tell whether Yapfast already runs");
            Start::Run(Instance::default())
        }
    }
}

impl Instance {
    /// Calls `opened` on a thread of its own for each later start that asks
    /// for the window, with the activation token the desktop gave that start.
    pub fn listen(&mut self, opened: impl Fn(Option<String>) + Send + 'static) {
        if let Some(listener) = self.listener.as_mut() {
            listener.listen(opened);
        }
    }
}

/// The token the desktop launched this start with, which lets the window
/// shown take the focus: Wayland's xdg-activation or X11's startup
/// notification.
fn activation_token() -> Option<String> {
    #[cfg(target_os = "linux")]
    for name in ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"] {
        if let Ok(token) = std::env::var(name)
            && !token.is_empty()
            && !token.contains(char::is_whitespace)
        {
            return Some(token);
        }
    }
    None
}

/// Asks the first Yapfast to show its window, waiting a little for it to listen.
fn hand(root: &Path, token: Option<&str>) -> bool {
    let started = std::time::Instant::now();
    loop {
        match ask(root, token) {
            Ok(()) => return true,
            Err(error) if started.elapsed() >= PATIENCE => {
                tracing::warn!(%error, "could not reach the running Yapfast");
                return false;
            }
            Err(_) => std::thread::sleep(RETRY),
        }
    }
}

#[cfg(unix)]
fn ask(root: &Path, token: Option<&str>) -> std::io::Result<()> {
    use std::io::Write;
    let mut stream = std::os::unix::net::UnixStream::connect(socket(root))?;
    match token {
        Some(token) => writeln!(stream, "open {token}"),
        None => writeln!(stream, "open"),
    }
}

/// The socket's path: beside the lock where it fits a socket's address, which
/// takes 103 bytes on macOS and 107 on Linux, or else named by a hash of the
/// folder in a short folder of the user's own.
#[cfg(unix)]
fn socket(root: &Path) -> std::path::PathBuf {
    let beside = root.join(SOCKET);
    if beside.as_os_str().len() < 104 {
        return beside;
    }
    let Some(folder) = short_folder() else {
        return beside;
    };
    use std::os::unix::ffi::OsStrExt;
    // The same folder by any path, as its lock is.
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a
    for byte in root.as_os_str().as_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    }
    folder.join(format!("yapfast-{hash:016x}.socket"))
}

/// macOS's cache folder for the user, under /var/folders: private, and
/// unlike the temporary folder beside it not emptied of files three days
/// old, as a socket a running Yapfast listens on would be.
#[cfg(target_os = "macos")]
fn short_folder() -> Option<std::path::PathBuf> {
    use std::{
        ffi::{OsStr, c_char, c_int},
        os::unix::ffi::OsStrExt,
    };
    const _CS_DARWIN_USER_CACHE_DIR: c_int = 65538;
    unsafe extern "C" {
        fn confstr(name: c_int, buffer: *mut c_char, length: usize) -> usize;
    }
    let mut buffer = [0u8; 1024];
    // SAFETY: confstr writes at most the buffer's length, NUL included, and
    // says how long the whole value is.
    let length = unsafe {
        confstr(
            _CS_DARWIN_USER_CACHE_DIR,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    (1..=buffer.len())
        .contains(&length)
        .then(|| OsStr::from_bytes(&buffer[..length - 1]).into())
}

/// Linux's runtime folder for the user, private and short.
#[cfg(all(unix, not(target_os = "macos")))]
fn short_folder() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|folder| !folder.is_empty())
        .map(Into::into)
}

#[cfg(unix)]
struct Listener(Option<std::os::unix::net::UnixListener>);

#[cfg(unix)]
impl Listener {
    fn bind(root: &Path) -> Option<Self> {
        let path = socket(root);
        // Holding the lock, this start is the only one: a socket there is a
        // gone Yapfast's.
        let _ = std::fs::remove_file(&path);
        match std::os::unix::net::UnixListener::bind(&path) {
            Ok(listener) => Some(Self(Some(listener))),
            Err(error) => {
                tracing::warn!(%error, "starting Yapfast again can't show this window");
                None
            }
        }
    }

    fn listen(&mut self, opened: impl Fn(Option<String>) + Send + 'static) {
        let Some(listener) = self.0.take() else {
            return;
        };
        let thread = std::thread::Builder::new()
            .name("yapfast-instance".into())
            .stack_size(64 * 1024)
            .spawn(move || {
                use std::io::Read;
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut request = String::new();
                    // An activation token is short; this caps a stranger's line.
                    if stream.take(4096).read_to_string(&mut request).is_err() {
                        continue;
                    }
                    let mut words = request.split_whitespace();
                    if words.next() == Some("open") {
                        opened(words.next().map(str::to_owned));
                    }
                }
            });
        if let Err(error) = thread {
            tracing::warn!(%error, "starting Yapfast again can't show this window");
        }
    }
}

#[cfg(windows)]
mod windows {
    //! A named auto-reset event for the folder: a later start sets it, and the
    //! first Yapfast's thread, waiting on it, shows the window. A second,
    //! unnamed event stops the thread before a restart starts the next process.

    use std::path::Path;

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        System::Threading::{
            CreateEventW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent,
            WaitForMultipleObjects,
        },
        UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow},
    };

    /// The event's name: `Local\yapfast-` and an FNV-1a hash of the folder's
    /// absolute path, as Windows compares paths, without case.
    fn name(root: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        let root = std::path::absolute(root).unwrap_or_else(|_| root.to_owned());
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for unit in root.as_os_str().encode_wide() {
            let unit = char::from_u32(unit.into()).map_or(unit, |c| c.to_ascii_lowercase() as u16);
            for byte in unit.to_le_bytes() {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
            }
        }
        format!("Local\\yapfast-{hash:016x}")
            .encode_utf16()
            .chain([0])
            .collect()
    }

    pub(super) fn ask(root: &Path, _token: Option<&str>) -> std::io::Result<()> {
        // SAFETY: the name is NUL-terminated; the handle is closed once set.
        unsafe {
            let event = OpenEventW(EVENT_MODIFY_STATE, 0, name(root).as_ptr());
            if event.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            // The started process may bring a window forward; it passes that on.
            AllowSetForegroundWindow(ASFW_ANY);
            let set = SetEvent(event);
            CloseHandle(event);
            if set == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// The two events, as addresses a thread can take.
    pub(super) struct Listener {
        event: usize,
        stop: usize,
        listening: bool,
    }

    impl Listener {
        pub(super) fn bind(root: &Path) -> Option<Self> {
            // SAFETY: the name is NUL-terminated; null attributes are the defaults.
            let (event, stop) = unsafe {
                (
                    CreateEventW(std::ptr::null(), 0, 0, name(root).as_ptr()),
                    CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()),
                )
            };
            if event.is_null() || stop.is_null() {
                tracing::warn!(
                    error = %std::io::Error::last_os_error(),
                    "starting Yapfast again can't show this window"
                );
                // SAFETY: each is a handle this made, or null.
                unsafe {
                    if !event.is_null() {
                        CloseHandle(event);
                    }
                    if !stop.is_null() {
                        CloseHandle(stop);
                    }
                }
                return None;
            }
            Some(Self {
                event: event as usize,
                stop: stop as usize,
                listening: false,
            })
        }

        pub(super) fn listen(&mut self, opened: impl Fn(Option<String>) + Send + 'static) {
            let (event, stop) = (self.event, self.stop);
            let thread = std::thread::Builder::new()
                .name("yapfast-instance".into())
                .stack_size(64 * 1024)
                .spawn(move || {
                    let handles = [event as HANDLE, stop as HANDLE];
                    // SAFETY: both handles stay open until this thread closes them.
                    while unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) }
                        == WAIT_OBJECT_0
                    {
                        opened(None);
                    }
                    // SAFETY: Drop only sets `stop` once this thread runs.
                    unsafe {
                        CloseHandle(handles[0]);
                        CloseHandle(handles[1]);
                    }
                });
            match thread {
                Ok(_) => self.listening = true,
                Err(error) => {
                    tracing::warn!(%error, "starting Yapfast again can't show this window");
                }
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // SAFETY: the handles are open: the thread closes them only after `stop`.
            unsafe {
                if self.listening {
                    SetEvent(self.stop as HANDLE);
                } else {
                    CloseHandle(self.event as HANDLE);
                    CloseHandle(self.stop as HANDLE);
                }
            }
        }
    }
}

#[cfg(windows)]
use windows::{Listener, ask};

/// The token a later start brought, for the window made next.
#[cfg(target_os = "linux")]
static TOKEN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The window made next takes the focus with `token`.
pub fn give_next_window(token: Option<String>) {
    #[cfg(target_os = "linux")]
    {
        *TOKEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = token;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = token;
}

/// Gives the window Slint makes the token a later start brought, if one waits.
pub fn attributes(
    attributes: slint::winit_030::winit::window::WindowAttributes,
) -> slint::winit_030::winit::window::WindowAttributes {
    #[cfg(target_os = "linux")]
    if let Some(token) = TOKEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        use slint::winit_030::winit::{
            platform::startup_notify::WindowAttributesExtStartupNotify, window::ActivationToken,
        };
        return attributes.with_activation_token(ActivationToken::from_raw(token));
    }
    attributes
}

/// Brings a shown window forward with the token a later start brought, on
/// Wayland, where winit can't give one to a window it already made.
#[cfg(target_os = "linux")]
pub fn activate(window: &slint::winit_030::winit::window::Window, token: &str) {
    use slint::winit_030::winit::raw_window_handle::{
        HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    };
    use wayland_client::{
        Connection, Dispatch, Proxy, QueueHandle,
        backend::{Backend, ObjectId},
        globals::{GlobalListContents, registry_queue_init},
        protocol::{wl_registry::WlRegistry, wl_surface::WlSurface},
    };
    use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

    struct State;
    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: <WlRegistry as Proxy>::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<XdgActivationV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &XdgActivationV1,
            _: <XdgActivationV1 as Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    let (Ok(display), Ok(handle)) = (window.display_handle(), window.window_handle()) else {
        return;
    };
    let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) =
        (display.as_raw(), handle.as_raw())
    else {
        return;
    };
    // SAFETY: winit's display and surface live while its window does, which
    // outlives this call.
    let connection = Connection::from_backend(unsafe {
        Backend::from_foreign_display(display.display.as_ptr().cast())
    });
    let surface =
        unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.surface.as_ptr().cast()) }
            .and_then(|id| WlSurface::from_id(&connection, id));
    let result = surface
        .map_err(|error| error.to_string())
        .and_then(|surface| {
            let (globals, queue) =
                registry_queue_init::<State>(&connection).map_err(|error| error.to_string())?;
            // A compositor without the protocol leaves focus to winit's request.
            let activation: XdgActivationV1 = match globals.bind(&queue.handle(), 1..=1, ()) {
                Ok(activation) => activation,
                Err(wayland_client::globals::BindError::NotPresent) => return Ok(()),
                Err(error) => return Err(error.to_string()),
            };
            activation.activate(token.to_owned(), &surface);
            activation.destroy();
            connection.flush().map_err(|error| error.to_string())
        });
    if let Err(error) = result {
        tracing::warn!(%error, "could not bring the window forward");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn opened(instance: &mut Instance) -> std::sync::mpsc::Receiver<Option<String>> {
        let (sender, receiver) = std::sync::mpsc::channel();
        instance.listen(move |token| {
            let _ = sender.send(token);
        });
        receiver
    }

    #[test]
    fn a_later_start_shows_the_first_window() {
        let root = tempfile::tempdir().unwrap();
        let Start::Run(mut first) = claim_with(root.path(), true, false, None) else {
            panic!("the first start runs");
        };
        let requests = opened(&mut first);
        assert!(matches!(
            claim_with(root.path(), true, false, Some("token-1".into())),
            Start::Handed
        ));
        let wait = Duration::from_secs(5);
        assert_eq!(
            requests.recv_timeout(wait).unwrap().as_deref(),
            Some("token-1")
        );
        assert!(matches!(
            claim_with(root.path(), true, false, None),
            Start::Handed
        ));
        assert_eq!(requests.recv_timeout(wait).unwrap(), None);
        // Starting at sign-in leaves the running one as it is.
        assert!(matches!(
            claim_with(root.path(), true, true, None),
            Start::Handed
        ));
        // Allowed, another runs beside the first, without listening.
        let Start::Run(another) = claim_with(root.path(), false, false, None) else {
            panic!("allowed, a later start runs");
        };
        assert!(another._lock.is_none() && another.listener.is_none());
        assert!(requests.recv_timeout(Duration::from_millis(200)).is_err());
        // Once the first ends, the next start is the first.
        drop(first);
        assert!(matches!(
            claim_with(root.path(), true, false, None),
            Start::Run(Instance {
                _lock: Some(_),
                listener: Some(_)
            })
        ));
    }

    /// A folder whose socket wouldn't fit beside the lock listens in a
    /// short folder, and a first Yapfast that doesn't answer keeps the
    /// folder its own.
    #[test]
    fn a_deep_folder_listens_elsewhere_and_stays_one() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("a-folder-deep-enough".repeat(4));
        let Start::Run(mut first) = claim_with(&root, true, false, None) else {
            panic!("the first start runs");
        };
        let path = socket(&root);
        assert!(root.join(SOCKET).as_os_str().len() >= 104);
        assert!(!path.starts_with(&root) && path.as_os_str().len() < 104);
        let requests = opened(&mut first);
        assert!(matches!(
            claim_with(&root, true, false, None),
            Start::Handed
        ));
        assert_eq!(requests.recv_timeout(Duration::from_secs(5)).unwrap(), None);

        let Start::Run(mut second) = claim_with(parent.path(), true, false, None) else {
            panic!("another folder runs");
        };
        second.listener = None;
        assert!(matches!(
            claim_with(parent.path(), true, false, None),
            Start::Handed
        ));
    }
}
