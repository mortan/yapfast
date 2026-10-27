//! The push-to-talk key, held in any app: through the GlobalShortcuts portal
//! on Wayland, XInput 2's raw key events on X11, a low-level keyboard hook on
//! Windows and a listen-only event tap on macOS. Where none of these works,
//! the key works while Yapfast's window has the focus, and settings say so.
//! Each tells [`yapfast_core::push_to_talk::hold`] when the key goes down or
//! comes up.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};

use serde::{Deserialize, Serialize};
use slint::winit_030::winit::{
    event::{ElementState, KeyEvent, WindowEvent},
    keyboard::{Key, KeyLocation, NamedKey, PhysicalKey},
    platform::scancode::PhysicalKeyExtScancode,
};
use yapfast_core::runtime::BackgroundRuntime;

/// A key recorded in settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keybind {
    /// winit's scan code for it: evdev's on Linux, which X11 reports 8
    /// higher, set 1's on Windows, with 0xE000 for an extended key, and the
    /// virtual key code (`kVK_*`) on macOS.
    pub scancode: u32,
    /// As settings show it, such as "V" or "Right Ctrl".
    pub name: String,
    /// Its XKB keysym name, such as "v" or "Control_R", which the
    /// GlobalShortcuts portal takes as the shortcut it would like.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
}

impl Keybind {
    /// The key a key press names, unless it has no scan code.
    pub fn recorded(event: &KeyEvent) -> Option<Self> {
        let scancode = event.physical_key.to_scancode()?;
        Some(Self {
            scancode,
            name: name(event),
            trigger: trigger(event),
        })
    }
}

/// The key without Shift or Caps Lock, so a letter reads as itself.
fn unmodified(event: &KeyEvent) -> Key {
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        use slint::winit_030::winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
        event.key_without_modifiers()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        event.logical_key.clone()
    }
}

fn name(event: &KeyEvent) -> String {
    let side = match event.location {
        KeyLocation::Left => "Left ",
        KeyLocation::Right => "Right ",
        KeyLocation::Numpad => "Numpad ",
        KeyLocation::Standard => "",
    };
    match unmodified(event) {
        Key::Character(text) => format!("{side}{}", text.to_uppercase()),
        Key::Named(named) => {
            let named = match named {
                NamedKey::Control => "Ctrl".to_owned(),
                NamedKey::Super | NamedKey::Meta => "Super".to_owned(),
                NamedKey::ArrowUp => "Up".to_owned(),
                NamedKey::ArrowDown => "Down".to_owned(),
                NamedKey::ArrowLeft => "Left".to_owned(),
                NamedKey::ArrowRight => "Right".to_owned(),
                NamedKey::Space => "Space".to_owned(),
                named => format!("{named:?}"),
            };
            format!("{side}{named}")
        }
        _ => match event.physical_key {
            PhysicalKey::Code(code) => format!("{code:?}"),
            PhysicalKey::Unidentified(_) => "Unknown key".to_owned(),
        },
    }
}

/// The key's keysym name, as the XDG shortcuts specification writes a
/// trigger; on Linux, where only the portal needs it.
#[cfg(target_os = "linux")]
fn trigger(event: &KeyEvent) -> Option<String> {
    let right = event.location == KeyLocation::Right;
    let side =
        |left: &str, right_name: &str| Some(if right { right_name } else { left }.to_owned());
    match unmodified(event) {
        Key::Character(text) => {
            let mut chars = text.chars();
            let (Some(char), None) = (chars.next(), chars.next()) else {
                return None;
            };
            let name = xkeysym::Keysym::from_char(char).name()?;
            Some(name.strip_prefix("XK_").unwrap_or(name).to_owned())
        }
        Key::Named(named) => match named {
            NamedKey::Control => side("Control_L", "Control_R"),
            NamedKey::Shift => side("Shift_L", "Shift_R"),
            NamedKey::Alt => side("Alt_L", "Alt_R"),
            NamedKey::Super | NamedKey::Meta => side("Super_L", "Super_R"),
            NamedKey::AltGraph => Some("ISO_Level3_Shift".to_owned()),
            NamedKey::CapsLock => Some("Caps_Lock".to_owned()),
            NamedKey::ScrollLock => Some("Scroll_Lock".to_owned()),
            NamedKey::NumLock => Some("Num_Lock".to_owned()),
            NamedKey::PageUp => Some("Prior".to_owned()),
            NamedKey::PageDown => Some("Next".to_owned()),
            NamedKey::Space => Some("space".to_owned()),
            NamedKey::Enter => Some("Return".to_owned()),
            NamedKey::Backspace => Some("BackSpace".to_owned()),
            NamedKey::ArrowUp => Some("Up".to_owned()),
            NamedKey::ArrowDown => Some("Down".to_owned()),
            NamedKey::ArrowLeft => Some("Left".to_owned()),
            NamedKey::ArrowRight => Some("Right".to_owned()),
            NamedKey::Tab
            | NamedKey::Insert
            | NamedKey::Delete
            | NamedKey::Home
            | NamedKey::End
            | NamedKey::Pause => Some(format!("{named:?}")),
            NamedKey::ContextMenu => Some("Menu".to_owned()),
            named => {
                let name = format!("{named:?}");
                // F1 to F35 are their own keysyms.
                (name.starts_with('F') && name[1..].parse::<u8>().is_ok()).then_some(name)
            }
        },
        _ => None,
    }
}

#[cfg(not(target_os = "linux"))]
fn trigger(_event: &KeyEvent) -> Option<String> {
    None
}

/// Where the key works.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Asking the desktop for it.
    Asking,
    /// In every app; with the shortcut the desktop gave it, in its own
    /// words, when it says.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    Everywhere(Option<String>),
    /// Only while Yapfast's window has the focus, for this reason, which
    /// the note goes on from.
    Focused(&'static str),
}

impl Reach {
    /// What settings say under the key.
    pub fn note(&self) -> String {
        match self {
            Reach::Asking => "Asking your desktop to let Yapfast use the key in every app...".into(),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            Reach::Everywhere(None) => "Hold the key in any app to talk.".into(),
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            Reach::Everywhere(Some(shortcut)) if shortcut.is_empty() => {
                "Your desktop hasn't allowed this shortcut yet. Approve it in your system's global shortcuts settings.".into()
            }
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            Reach::Everywhere(Some(shortcut)) => format!(
                "Your desktop gave push to talk the shortcut {shortcut}. Hold it in any app to talk; change it in your system's global shortcuts settings."
            ),
            Reach::Focused(why) => format!(
                "{why}, so push to talk works only while Yapfast's window is in front."
            ),
        }
    }
}

/// The key's scan code while the window's own key events stand in for a
/// global key; [`NONE`] otherwise.
static FOCUSED_KEY: AtomicU32 = AtomicU32::new(NONE);
const NONE: u32 = u32::MAX;

/// Holds the key from the window's own events while no global key works.
/// Losing the focus lets it go, as the key's release won't come.
pub fn window_event(event: &WindowEvent) {
    let key = FOCUSED_KEY.load(Ordering::Relaxed);
    if key == NONE {
        return;
    }
    match event {
        WindowEvent::KeyboardInput { event, .. }
            if !event.repeat && event.physical_key.to_scancode() == Some(key) =>
        {
            yapfast_core::push_to_talk::hold(event.state == ElementState::Pressed);
        }
        WindowEvent::Focused(false) => yapfast_core::push_to_talk::hold(false),
        _ => {}
    }
}

/// Watches the key in every app it can, until dropped.
pub struct Watcher {
    key: Keybind,
    reach: Arc<Mutex<Reach>>,
    #[cfg(target_os = "linux")]
    _global: Option<linux::Global>,
    #[cfg(windows)]
    _global: Option<windows::Hook>,
    #[cfg(target_os = "macos")]
    _global: Option<macos::Tap>,
}

impl Watcher {
    /// Starts watching `key`; `told` hears where it works whenever that
    /// changes, from any thread.
    pub fn start(
        key: Keybind,
        runtime: &BackgroundRuntime,
        told: impl Fn(Reach) + Send + Sync + 'static,
    ) -> Self {
        let reach = Arc::new(Mutex::new(Reach::Asking));
        let shared = reach.clone();
        let scancode = key.scancode;
        let report = Arc::new(move |now: Reach| {
            let focused = matches!(now, Reach::Focused(_));
            FOCUSED_KEY.store(if focused { scancode } else { NONE }, Ordering::Relaxed);
            *shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = now.clone();
            told(now);
        });
        #[cfg(target_os = "linux")]
        let global = linux::Global::start(&key, runtime, report);
        #[cfg(windows)]
        let global = {
            let _ = runtime;
            windows::Hook::start(scancode, report)
        };
        #[cfg(target_os = "macos")]
        let global = {
            let _ = runtime;
            macos::Tap::start(scancode, report)
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            let _ = runtime;
            report(Reach::Focused(
                "Yapfast can't watch keys in other apps on this system yet",
            ));
        }
        Self {
            key,
            reach,
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            _global: global,
        }
    }

    pub fn key(&self) -> &Keybind {
        &self.key
    }

    pub fn reach(&self) -> Reach {
        self.reach
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        FOCUSED_KEY.store(NONE, Ordering::Relaxed);
        yapfast_core::push_to_talk::hold(false);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
type Report = Arc<dyn Fn(Reach) + Send + Sync>;

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use anyhow::{Context, Result, anyhow, bail};
    use futures_util::StreamExt;
    use x11rb::{
        connection::Connection as _,
        protocol::{
            Event,
            xinput::{self, ConnectionExt as _},
            xproto::{self, ConnectionExt as _},
        },
        rust_connection::RustConnection,
    };
    use yapfast_core::runtime::{BackgroundRuntime, BackgroundTask};
    use zbus::{
        Connection, MessageStream,
        zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
    };

    use super::{Keybind, Reach, Report};

    /// A global key on Wayland or X11; stops when dropped.
    pub enum Global {
        Portal { _session: BackgroundTask<()> },
        X11 { _events: X11 },
    }

    impl Global {
        /// Wayland's desktops grab keys through the portal; X11 lets any
        /// client hear every key.
        pub fn start(key: &Keybind, runtime: &BackgroundRuntime, report: Report) -> Option<Self> {
            let wayland =
                std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
            if wayland {
                report(Reach::Asking);
                let trigger = key.trigger.clone();
                return Some(Global::Portal {
                    _session: runtime.spawn(async move {
                        if let Err(error) = portal(trigger, &report).await {
                            tracing::info!(
                                error = format!("{error:#}"),
                                "no global push-to-talk key through the portal"
                            );
                            report(Reach::Focused(
                                "Your desktop doesn't offer Yapfast a global shortcut",
                            ));
                        }
                    }),
                });
            }
            match X11::start(key.scancode, report.clone()) {
                Ok(x11) => {
                    report(Reach::Everywhere(None));
                    Some(Global::X11 { _events: x11 })
                }
                Err(error) => {
                    tracing::info!(
                        error = format!("{error:#}"),
                        "no global push-to-talk key on X11"
                    );
                    report(Reach::Focused("Yapfast couldn't watch keys in other apps"));
                    None
                }
            }
        }
    }

    const PORTAL: &str = "org.freedesktop.portal.Desktop";
    const PATH: &str = "/org/freedesktop/portal/desktop";
    const SHORTCUTS: &str = "org.freedesktop.portal.GlobalShortcuts";
    /// The one shortcut Yapfast binds.
    const SHORTCUT: &str = "push-to-talk";

    /// Binds the key through the GlobalShortcuts portal and holds it as the
    /// desktop says, until the task is dropped, which ends the session with
    /// the connection.
    async fn portal(trigger: Option<String>, report: &Report) -> Result<()> {
        let connection = Connection::session().await?;
        // Names Yapfast to the portal, which outside a sandbox can't tell
        // the app otherwise; older portals don't have it.
        let _ = connection
            .call_method(
                Some(PORTAL),
                PATH,
                Some("org.freedesktop.host.portal.Registry"),
                "Register",
                &(crate::icon::APP_ID, HashMap::<&str, Value>::new()),
            )
            .await;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(PORTAL)?
            .build();
        let mut signals = MessageStream::for_match_rule(rule, &connection, Some(16)).await?;
        let sender = connection
            .unique_name()
            .context("no name on the session bus")?
            .trim_start_matches(':')
            .replace('.', "_");
        let token = format!("yapfast_{}", std::process::id());
        let request = |step: &str| format!("{PATH}/request/{sender}/{token}_{step}");

        let options = HashMap::from([
            ("handle_token", Value::from(format!("{token}_session"))),
            ("session_handle_token", Value::from(token.clone())),
        ]);
        connection
            .call_method(
                Some(PORTAL),
                PATH,
                Some(SHORTCUTS),
                "CreateSession",
                &(options,),
            )
            .await?;
        let results = response(&mut signals, &request("session")).await?;
        let session = match results.get("session_handle").map(|value| &**value) {
            Some(Value::Str(handle)) => OwnedObjectPath::try_from(handle.as_str())?,
            Some(Value::ObjectPath(handle)) => handle.clone().into(),
            _ => bail!("the portal gave no session"),
        };

        let mut wanted = HashMap::from([("description", Value::from("Push to talk"))]);
        if let Some(trigger) = &trigger {
            wanted.insert("preferred_trigger", Value::from(trigger.as_str()));
        }
        let options = HashMap::from([("handle_token", Value::from(format!("{token}_bind")))]);
        connection
            .call_method(
                Some(PORTAL),
                PATH,
                Some(SHORTCUTS),
                "BindShortcuts",
                &(&session, vec![(SHORTCUT, wanted)], "", options),
            )
            .await?;
        let results = response(&mut signals, &request("bind")).await?;
        report(Reach::Everywhere(Some(described(&results))));

        while let Some(message) = signals.next().await {
            let message = message?;
            let header = message.header();
            let held = match header.member().map(|member| member.as_str()) {
                Some("Activated") => true,
                Some("Deactivated") => false,
                Some("ShortcutsChanged") => {
                    if let Ok((path, shortcuts)) = message
                        .body()
                        .deserialize::<(ObjectPath, Vec<(String, HashMap<String, OwnedValue>)>)>()
                        && path == *session
                    {
                        report(Reach::Everywhere(Some(trigger_description(&shortcuts))));
                    }
                    continue;
                }
                Some("Closed") if header.path().is_some_and(|path| *path == *session) => {
                    bail!("the desktop closed the shortcut's session");
                }
                _ => continue,
            };
            if let Ok((path, id, ..)) =
                message
                    .body()
                    .deserialize::<(ObjectPath, &str, u64, HashMap<String, OwnedValue>)>()
                && path == *session
                && id == SHORTCUT
            {
                yapfast_core::push_to_talk::hold(held);
            }
        }
        Err(anyhow!("the session bus closed"))
    }

    /// Waits for the portal's answer to a request, at its object path.
    async fn response(
        signals: &mut MessageStream,
        request: &str,
    ) -> Result<HashMap<String, OwnedValue>> {
        while let Some(message) = signals.next().await {
            let message = message?;
            let header = message.header();
            if header
                .member()
                .is_none_or(|member| member.as_str() != "Response")
                || header.path().is_none_or(|path| path.as_str() != request)
            {
                continue;
            }
            let (code, results) = message
                .body()
                .deserialize::<(u32, HashMap<String, OwnedValue>)>()?;
            return match code {
                0 => Ok(results),
                1 => Err(anyhow!("the shortcut was declined")),
                _ => Err(anyhow!("the portal failed ({code})")),
            };
        }
        Err(anyhow!("the session bus closed"))
    }

    /// The shortcut the desktop bound, in its words; empty when it bound
    /// none yet.
    fn described(results: &HashMap<String, OwnedValue>) -> String {
        results
            .get("shortcuts")
            .and_then(|value| {
                Vec::<(String, HashMap<String, OwnedValue>)>::try_from(value.try_clone().ok()?).ok()
            })
            .map(|shortcuts| trigger_description(&shortcuts))
            .unwrap_or_default()
    }

    fn trigger_description(shortcuts: &[(String, HashMap<String, OwnedValue>)]) -> String {
        shortcuts
            .iter()
            .find(|(id, _)| id == SHORTCUT)
            .and_then(|(_, details)| {
                match details.get("trigger_description").map(|value| &**value) {
                    Some(Value::Str(text)) => Some(text.to_string()),
                    _ => None,
                }
            })
            .unwrap_or_default()
    }

    /// XInput 2's raw key events from the root window, which come whatever
    /// window has the focus and leave the key to it. A thread waits on them.
    pub struct X11 {
        connection: Arc<RustConnection>,
        window: xproto::Window,
        stop: Arc<AtomicBool>,
    }

    impl X11 {
        fn start(scancode: u32, report: Report) -> Result<Self> {
            let (connection, screen) = x11rb::connect(None)?;
            let root = connection.setup().roots[screen].root;
            let version = connection.xinput_xi_query_version(2, 2)?.reply()?;
            if version.major_version < 2 {
                bail!("the X server has no XInput 2");
            }
            // A window of its own to wake the thread with when stopping.
            let window = connection.generate_id()?;
            connection.create_window(
                0,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                xproto::WindowClass::INPUT_ONLY,
                0,
                &xproto::CreateWindowAux::new(),
            )?;
            connection.xinput_xi_select_events(
                root,
                &[xinput::EventMask {
                    deviceid: xinput::Device::ALL_MASTER.into(),
                    mask: vec![
                        xinput::XIEventMask::RAW_KEY_PRESS | xinput::XIEventMask::RAW_KEY_RELEASE,
                    ],
                }],
            )?;
            connection.flush()?;
            let connection = Arc::new(connection);
            let stop = Arc::new(AtomicBool::new(false));
            let (waiting, stopped) = (connection.clone(), stop.clone());
            // X11 keycodes are evdev's plus 8.
            let keycode = scancode + 8;
            std::thread::Builder::new()
                .name("yapfast-talk-key".into())
                .spawn(move || {
                    while !stopped.load(Ordering::Relaxed) {
                        match waiting.wait_for_event() {
                            Ok(Event::XinputRawKeyPress(event)) if event.detail == keycode => {
                                yapfast_core::push_to_talk::hold(true);
                            }
                            Ok(Event::XinputRawKeyRelease(event)) if event.detail == keycode => {
                                yapfast_core::push_to_talk::hold(false);
                            }
                            Ok(_) => {}
                            Err(error) => {
                                tracing::warn!(%error, "lost the X server's keys");
                                yapfast_core::push_to_talk::hold(false);
                                report(Reach::Focused("Yapfast lost the X server's keys"));
                                break;
                            }
                        }
                    }
                })?;
            Ok(Self {
                connection,
                window,
                stop,
            })
        }
    }

    impl Drop for X11 {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let wake =
                xproto::ClientMessageEvent::new(32, self.window, xproto::AtomEnum::NONE, [0u32; 5]);
            let _ =
                self.connection
                    .send_event(false, self.window, xproto::EventMask::NO_EVENT, wake);
            let _ = self.connection.flush();
        }
    }

    #[cfg(test)]
    mod tests {
        use std::time::{Duration, Instant};

        use super::*;

        /// Waits up to two seconds for the key to be `held` or not.
        fn wait_held(held: bool) {
            let until = Instant::now() + Duration::from_secs(2);
            while yapfast_core::push_to_talk::held() != held {
                assert!(Instant::now() < until, "the key wasn't held: {held}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// Pressing the key in another client holds it. Needs `DISPLAY` on a
        /// private X server, such as `Xvfb :57`, and `xdotool`, which types
        /// through XTEST as a keyboard would.
        #[test]
        #[ignore = "needs DISPLAY on a private X server and xdotool"]
        fn x11_hears_the_key_whatever_has_the_focus() {
            let x11 = X11::start(47, Arc::new(|_| {})).unwrap(); // evdev's V
            let xdotool = |action| {
                let status = std::process::Command::new("xdotool")
                    .args([action, "v"])
                    .status()
                    .unwrap();
                assert!(status.success());
            };
            xdotool("keydown");
            wait_held(true);
            xdotool("keyup");
            wait_held(false);
            // Another key leaves it alone.
            std::process::Command::new("xdotool")
                .args(["key", "b"])
                .status()
                .unwrap();
            std::thread::sleep(Duration::from_millis(100));
            assert!(!yapfast_core::push_to_talk::held());
            drop(x11);
        }

        /// A stand-in for xdg-desktop-portal's GlobalShortcuts: it answers
        /// each request with Response on the request's path, binds the
        /// shortcut to what was preferred, and then presses it.
        struct FakePortal;

        #[zbus::interface(name = "org.freedesktop.portal.GlobalShortcuts")]
        impl FakePortal {
            async fn create_session(
                &self,
                options: HashMap<String, OwnedValue>,
                #[zbus(header)] header: zbus::message::Header<'_>,
                #[zbus(connection)] connection: &Connection,
            ) -> OwnedObjectPath {
                let request = request_path(&header, &options);
                let session = format!("{PATH}/session/test/1");
                let results = HashMap::from([("session_handle", Value::from(session))]);
                respond(connection, &request, results).await;
                request
            }

            async fn bind_shortcuts(
                &self,
                session: OwnedObjectPath,
                shortcuts: Vec<(String, HashMap<String, OwnedValue>)>,
                _parent_window: String,
                options: HashMap<String, OwnedValue>,
                #[zbus(header)] header: zbus::message::Header<'_>,
                #[zbus(connection)] connection: &Connection,
            ) -> OwnedObjectPath {
                let request = request_path(&header, &options);
                let (id, details) = &shortcuts[0];
                let Some(Value::Str(trigger)) = details.get("preferred_trigger").map(|v| &**v)
                else {
                    panic!("no preferred trigger");
                };
                let bound = vec![(
                    id.clone(),
                    HashMap::from([(
                        "trigger_description".to_owned(),
                        Value::from(trigger.to_uppercase()),
                    )]),
                )];
                respond(
                    connection,
                    &request,
                    HashMap::from([("shortcuts", Value::from(bound))]),
                )
                .await;
                let connection = connection.clone();
                let (session, id) = (session.clone(), id.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    zbus::block_on(press(&connection, &session, &id, "Activated"));
                });
                request
            }
        }

        fn request_path(
            header: &zbus::message::Header<'_>,
            options: &HashMap<String, OwnedValue>,
        ) -> OwnedObjectPath {
            let sender = header
                .sender()
                .unwrap()
                .trim_start_matches(':')
                .replace('.', "_");
            let Some(Value::Str(token)) = options.get("handle_token").map(|v| &**v) else {
                panic!("no handle token");
            };
            OwnedObjectPath::try_from(format!("{PATH}/request/{sender}/{token}")).unwrap()
        }

        async fn respond(
            connection: &Connection,
            request: &OwnedObjectPath,
            results: HashMap<&str, Value<'_>>,
        ) {
            connection
                .emit_signal(
                    None::<()>,
                    request,
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(0u32, results),
                )
                .await
                .unwrap();
        }

        async fn press(connection: &Connection, session: &OwnedObjectPath, id: &str, signal: &str) {
            let options = HashMap::<&str, Value>::new();
            connection
                .emit_signal(
                    None::<()>,
                    PATH,
                    SHORTCUTS,
                    signal,
                    &(session, id, 0u64, options),
                )
                .await
                .unwrap();
        }

        /// The portal's handshake, and its shortcut going down and up. Needs
        /// a private session bus, as `dbus-run-session` gives.
        #[test]
        #[ignore = "needs a private session bus"]
        fn the_portal_binds_the_key_and_holds_it() {
            let server = zbus::blocking::connection::Builder::session()
                .unwrap()
                .name(PORTAL)
                .unwrap()
                .serve_at(PATH, FakePortal)
                .unwrap()
                .build()
                .unwrap();
            let told = Arc::new(std::sync::Mutex::new(Vec::new()));
            let heard = told.clone();
            let report: Report = Arc::new(move |reach| heard.lock().unwrap().push(reach));
            let runtime = BackgroundRuntime::new().unwrap();
            let _task = runtime.spawn(async move { portal(Some("v".into()), &report).await });
            let until = Instant::now() + Duration::from_secs(5);
            while !yapfast_core::push_to_talk::held() {
                assert!(
                    Instant::now() < until,
                    "the shortcut wasn't held: {:?}",
                    told.lock().unwrap()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(*told.lock().unwrap(), [Reach::Everywhere(Some("V".into()))]);
            let session = OwnedObjectPath::try_from(format!("{PATH}/session/test/1")).unwrap();
            zbus::block_on(press(server.inner(), &session, SHORTCUT, "Deactivated"));
            wait_held(false);
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::{
        ptr,
        sync::{
            atomic::{AtomicU32, Ordering},
            mpsc,
        },
    };

    use windows_sys::Win32::{
        Foundation::{LPARAM, LRESULT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::WindowsAndMessaging::{
            CallNextHookEx, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG,
            PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN,
            WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
        },
    };

    use super::{Reach, Report};

    /// The key the hook watches, as winit's scan code.
    static KEY: AtomicU32 = AtomicU32::new(super::NONE);

    /// A low-level keyboard hook on a thread of its own, which sees every
    /// key, its release included, and passes each on. Removed when dropped.
    pub struct Hook {
        thread: u32,
    }

    impl Hook {
        pub fn start(scancode: u32, report: Report) -> Option<Self> {
            KEY.store(scancode, Ordering::Relaxed);
            let (started, thread) = mpsc::sync_channel(1);
            let spawned = std::thread::Builder::new()
                .name("yapfast-talk-key".into())
                .spawn(move || {
                    // SAFETY: the hook procedure below only reads the event
                    // Windows hands it; the message loop runs on this thread,
                    // which the hook needs, until WM_QUIT.
                    unsafe {
                        let hook = SetWindowsHookExW(
                            WH_KEYBOARD_LL,
                            Some(proc),
                            GetModuleHandleW(ptr::null()),
                            0,
                        );
                        if hook.is_null() {
                            let _ = started.send(None);
                            return;
                        }
                        let _ = started.send(Some(GetCurrentThreadId()));
                        let mut message: MSG = std::mem::zeroed();
                        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {}
                        UnhookWindowsHookEx(hook);
                    }
                });
            let thread = spawned.ok().and_then(|_| thread.recv().ok().flatten());
            match thread {
                Some(thread) => {
                    report(Reach::Everywhere(None));
                    Some(Self { thread })
                }
                None => {
                    report(Reach::Focused(
                        "Windows didn't let Yapfast watch keys in other apps",
                    ));
                    None
                }
            }
        }
    }

    impl Drop for Hook {
        fn drop(&mut self) {
            KEY.store(super::NONE, Ordering::Relaxed);
            // SAFETY: posting to a thread id is harmless if it has ended.
            unsafe {
                PostThreadMessageW(self.thread, WM_QUIT, 0, 0);
            }
        }
    }

    /// Notes the push-to-talk key going down or up, and passes every key on.
    unsafe extern "system" fn proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY: for HC_ACTION, lparam points at the key's details.
            let key = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            let extended = if key.flags & LLKHF_EXTENDED != 0 {
                0xE000
            } else {
                0
            };
            if key.scanCode | extended == KEY.load(Ordering::Relaxed) {
                match wparam as u32 {
                    WM_KEYDOWN | WM_SYSKEYDOWN => yapfast_core::push_to_talk::hold(true),
                    WM_KEYUP | WM_SYSKEYUP => yapfast_core::push_to_talk::hold(false),
                    _ => {}
                }
            }
        }
        // SAFETY: the next hook gets the event as it came.
        unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
    }

    #[cfg(test)]
    mod tests {
        use std::{
            sync::Arc,
            time::{Duration, Instant},
        };

        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
            SendInput,
        };

        use super::*;

        /// Types a key by its scan code, as a keyboard would.
        fn send(scancode: u16, up: bool) {
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: 0,
                        wScan: scancode,
                        dwFlags: KEYEVENTF_SCANCODE | if up { KEYEVENTF_KEYUP } else { 0 },
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            // SAFETY: one input, of the size Windows expects.
            let sent = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
            assert_eq!(sent, 1);
        }

        fn wait_held(held: bool) {
            let until = Instant::now() + Duration::from_secs(2);
            while yapfast_core::push_to_talk::held() != held {
                assert!(Instant::now() < until, "the key wasn't held: {held}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// The hook hears the key typed anywhere, down and up. Run it alone,
        /// on a desktop, or under Wine with an X server.
        #[test]
        #[ignore = "types into the desktop"]
        fn the_hook_hears_the_key_go_down_and_up() {
            let hook = Hook::start(0x2F, Arc::new(|_| {})).expect("no hook"); // V
            send(0x2F, false);
            wait_held(true);
            send(0x2F, true);
            wait_held(false);
            send(0x30, false); // B
            send(0x30, true);
            std::thread::sleep(Duration::from_millis(100));
            assert!(!yapfast_core::push_to_talk::held());
            drop(hook);
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{cell::Cell, ffi::c_void, ptr, sync::mpsc};

    use super::{Reach, Report};

    type CFTypeRef = *const c_void;
    type CFRunLoopRef = *mut c_void;
    type CFMachPortRef = *mut c_void;
    type CGEventRef = *mut c_void;
    type CGEventTapCallBack = unsafe extern "C" fn(
        proxy: *mut c_void,
        kind: u32,
        event: CGEventRef,
        user_info: *mut c_void,
    ) -> CGEventRef;

    const SESSION_EVENT_TAP: u32 = 1; // kCGSessionEventTap
    const HEAD_INSERT: u32 = 0; // kCGHeadInsertEventTap
    const LISTEN_ONLY: u32 = 1; // kCGEventTapOptionListenOnly
    const KEY_DOWN: u32 = 10; // kCGEventKeyDown
    const KEY_UP: u32 = 11; // kCGEventKeyUp
    const FLAGS_CHANGED: u32 = 12; // kCGEventFlagsChanged
    const DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE; // kCGEventTapDisabledByTimeout
    const DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF; // kCGEventTapDisabledByUserInput
    const KEYCODE_FIELD: u32 = 9; // kCGKeyboardEventKeycode

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightListenEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: CGEventTapCallBack,
            user_info: *mut c_void,
        ) -> CFMachPortRef;
        fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
        fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
        fn CGEventGetFlags(event: CGEventRef) -> u64;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopCommonModes: CFTypeRef;
        fn CFRetain(object: CFTypeRef) -> CFTypeRef;
        fn CFRelease(object: CFTypeRef);
        fn CFMachPortCreateRunLoopSource(
            allocator: CFTypeRef,
            port: CFMachPortRef,
            order: isize,
        ) -> *mut c_void;
        fn CFMachPortInvalidate(port: CFMachPortRef);
        fn CFRunLoopGetCurrent() -> CFRunLoopRef;
        fn CFRunLoopAddSource(run_loop: CFRunLoopRef, source: *mut c_void, mode: CFTypeRef);
        fn CFRunLoopRun();
        fn CFRunLoopStop(run_loop: CFRunLoopRef);
    }

    thread_local! {
        /// The tap of the thread it runs on, for its callback to re-enable.
        static PORT: Cell<CFMachPortRef> = const { Cell::new(ptr::null_mut()) };
    }

    /// The tap's port and its thread's run loop, each retained for the
    /// [`Tap`]; Core Foundation lets other threads invalidate and stop them.
    struct Running {
        port: CFMachPortRef,
        run_loop: CFRunLoopRef,
    }

    // SAFETY: both are Core Foundation objects, whose retain counts are
    // atomic, and `Tap` only invalidates, stops and releases them.
    unsafe impl Send for Running {}

    /// A listen-only event tap on a thread of its own, which sees every
    /// key going down and up, modifiers included, while another app is in
    /// front, and takes none away. It needs Input Monitoring. Removed when
    /// dropped. Not `RegisterEventHotKey`, which keeps the key from the
    /// app in front and doesn't tell its release.
    pub struct Tap(Running);

    impl Tap {
        pub fn start(keycode: u32, report: Report) -> Option<Self> {
            // SAFETY: plain calls, without arguments. The request asks
            // macOS to list Yapfast under Input Monitoring, showing its
            // prompt the first time; it applies from the next start.
            if !unsafe { CGPreflightListenEventAccess() } {
                unsafe { CGRequestListenEventAccess() };
                report(Reach::Focused(
                    "Allow Yapfast in System Settings › Privacy & Security › Input Monitoring and start it again; until then macOS keeps other apps' keys from it",
                ));
                return None;
            }
            let (started, running) = mpsc::sync_channel(1);
            let spawned = std::thread::Builder::new()
                .name("yapfast-talk-key".into())
                .spawn(move || {
                    let mask = 1 << KEY_DOWN | 1 << KEY_UP | 1 << FLAGS_CHANGED;
                    // SAFETY: the callback only reads the events it's
                    // given and the key code it's given as user data; the
                    // port, its source and the run loop are this thread's,
                    // retained once more for the `Tap` before it hears of
                    // them, and released here once the run loop stops.
                    unsafe {
                        let port = CGEventTapCreate(
                            SESSION_EVENT_TAP,
                            HEAD_INSERT,
                            LISTEN_ONLY,
                            mask,
                            tapped,
                            keycode as usize as *mut c_void,
                        );
                        if port.is_null() {
                            let _ = started.send(None);
                            return;
                        }
                        PORT.set(port);
                        let source = CFMachPortCreateRunLoopSource(ptr::null(), port, 0);
                        let run_loop = CFRunLoopGetCurrent();
                        CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
                        CGEventTapEnable(port, true);
                        CFRetain(port);
                        CFRetain(run_loop);
                        let _ = started.send(Some(Running { port, run_loop }));
                        // Returns once stopped, or at once if the port was
                        // invalidated before, which takes its source away.
                        CFRunLoopRun();
                        CFRelease(source);
                        CFRelease(port);
                    }
                });
            match spawned.ok().and_then(|_| running.recv().ok().flatten()) {
                Some(running) => {
                    report(Reach::Everywhere(None));
                    Some(Self(running))
                }
                None => {
                    report(Reach::Focused(
                        "macOS didn't let Yapfast watch keys in other apps",
                    ));
                    None
                }
            }
        }
    }

    impl Drop for Tap {
        fn drop(&mut self) {
            // SAFETY: the `Tap` holds a reference to each, which it gives
            // up here; invalidating the port takes its source off the run
            // loop, so the thread ends even if it hasn't started running.
            unsafe {
                CFMachPortInvalidate(self.0.port);
                CFRunLoopStop(self.0.run_loop);
                CFRelease(self.0.port);
                CFRelease(self.0.run_loop);
            }
        }
    }

    /// Notes the push-to-talk key going down or up, re-enabling the tap
    /// when macOS turned it off, and passes the event on as it came.
    unsafe extern "C" fn tapped(
        _proxy: *mut c_void,
        kind: u32,
        event: CGEventRef,
        user_info: *mut c_void,
    ) -> CGEventRef {
        match kind {
            // macOS turns a tap off when a callback is slow, or for secure
            // input, such as a password field; the key may have come up
            // unseen meanwhile.
            DISABLED_BY_TIMEOUT | DISABLED_BY_USER_INPUT => {
                yapfast_core::push_to_talk::hold(false);
                let port = PORT.get();
                if !port.is_null() {
                    // SAFETY: this thread's tap, alive while its run loop
                    // calls this.
                    unsafe { CGEventTapEnable(port, true) };
                }
            }
            _ => {
                // SAFETY: for key and flag events, `event` is the event.
                let (code, flags) = unsafe {
                    (
                        CGEventGetIntegerValueField(event, KEYCODE_FIELD),
                        CGEventGetFlags(event),
                    )
                };
                if let Some(held) = held(kind, code, flags, user_info as usize as u32) {
                    yapfast_core::push_to_talk::hold(held);
                }
            }
        }
        event
    }

    /// Whether an event holds the key `keycode`, lets it go, or neither:
    /// keys go down and up; a modifier's FlagsChanged has its flag set
    /// while it's held, the side's own where there are two.
    fn held(kind: u32, code: i64, flags: u64, keycode: u32) -> Option<bool> {
        if code != i64::from(keycode) {
            return None;
        }
        match kind {
            KEY_DOWN => Some(true),
            KEY_UP => Some(false),
            FLAGS_CHANGED => {
                let flag = match keycode {
                    0x3B => 0x0000_0001, // Left Control: NX_DEVICELCTLKEYMASK
                    0x38 => 0x0000_0002, // Left Shift
                    0x3C => 0x0000_0004, // Right Shift
                    0x37 => 0x0000_0008, // Left Command
                    0x36 => 0x0000_0010, // Right Command
                    0x3A => 0x0000_0020, // Left Option
                    0x3D => 0x0000_0040, // Right Option
                    0x3E => 0x0000_2000, // Right Control
                    0x39 => 0x0001_0000, // Caps Lock: kCGEventFlagMaskAlphaShift, its lock
                    0x3F => 0x0080_0000, // Fn: kCGEventFlagMaskSecondaryFn
                    _ => return None,
                };
                Some(flags & flag != 0)
            }
            _ => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use std::{
            sync::Arc,
            time::{Duration, Instant},
        };

        use super::*;

        const HID_EVENT_TAP: u32 = 0; // kCGHIDEventTap

        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGEventCreateKeyboardEvent(source: CFTypeRef, key: u16, down: bool) -> CGEventRef;
            fn CGEventSetType(event: CGEventRef, kind: u32);
            fn CGEventSetFlags(event: CGEventRef, flags: u64);
            fn CGEventPost(tap: u32, event: CGEventRef);
        }

        /// Types a key by its virtual key code, as a keyboard would; a
        /// modifier as FlagsChanged with `flags`.
        fn send(keycode: u16, down: bool, modifier: Option<u64>) {
            // SAFETY: an event of our own, posted and released.
            unsafe {
                let event = CGEventCreateKeyboardEvent(ptr::null(), keycode, down);
                assert!(!event.is_null());
                if let Some(flags) = modifier {
                    CGEventSetType(event, FLAGS_CHANGED);
                    CGEventSetFlags(event, flags);
                }
                CGEventPost(HID_EVENT_TAP, event);
                CFRelease(event);
            }
        }

        fn wait_held(held: bool) {
            let until = Instant::now() + Duration::from_secs(2);
            while yapfast_core::push_to_talk::held() != held {
                assert!(Instant::now() < until, "the key wasn't held: {held}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        #[test]
        fn keys_and_modifiers_go_down_and_up() {
            const V: u32 = 0x09;
            const RIGHT_SHIFT: u32 = 0x3C;
            assert_eq!(held(KEY_DOWN, 0x09, 0, V), Some(true));
            assert_eq!(held(KEY_UP, 0x09, 0, V), Some(false));
            assert_eq!(held(KEY_DOWN, 0x0B, 0, V), None); // B
            // Right Shift down: the generic Shift flag and the right side's.
            assert_eq!(held(FLAGS_CHANGED, 0x3C, 0x2_0004, RIGHT_SHIFT), Some(true));
            // Left Shift going down while the right one is held.
            assert_eq!(held(FLAGS_CHANGED, 0x38, 0x2_0006, RIGHT_SHIFT), None);
            // Right Shift up with the left one still down.
            assert_eq!(
                held(FLAGS_CHANGED, 0x3C, 0x2_0002, RIGHT_SHIFT),
                Some(false)
            );
        }

        /// The tap hears the key typed anywhere, down and up. Needs Input
        /// Monitoring to hear and Accessibility to type, for the terminal
        /// it runs from; macOS denies both without asking under tmux.
        #[test]
        #[ignore = "types into the desktop; needs Input Monitoring and Accessibility"]
        fn the_tap_hears_the_key_go_down_and_up() {
            let told = Arc::new(std::sync::Mutex::new(Vec::new()));
            let heard = told.clone();
            let tap = Tap::start(
                0x09,
                Arc::new(move |reach| heard.lock().unwrap().push(reach)),
            );
            let tap = tap.unwrap_or_else(|| panic!("no tap: {:?}", told.lock().unwrap()));
            send(0x09, true, None); // V
            wait_held(true);
            send(0x09, false, None);
            wait_held(false);
            send(0x0B, true, None); // B
            send(0x0B, false, None);
            std::thread::sleep(Duration::from_millis(100));
            assert!(!yapfast_core::push_to_talk::held());
            drop(tap);

            let tap = Tap::start(0x3C, Arc::new(|_| {})).expect("no tap"); // Right Shift
            send(0x3C, true, Some(0x2_0004));
            wait_held(true);
            send(0x3C, false, Some(0));
            wait_held(false);
            drop(tap);
        }
    }
}
