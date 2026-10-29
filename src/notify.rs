//! Desktop notifications. On Linux they go to the freedesktop notification service,
//! which GNOME, KDE and the other desktops run; on macOS to the User Notifications
//! framework, for Yapfast.app. Clicking one opens its conversation. Windows doesn't
//! show them yet.

use yapfast_core::chat::Alert;

/// How a notification shows.
#[derive(Clone, Copy)]
pub struct Style {
    /// Say what the message says, rather than only who wrote.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub text: bool,
}

/// Shows notifications. On Linux from a thread of its own, so a slow notification
/// service never holds up the window; macOS's framework answers asynchronously itself.
pub struct Notifier {
    #[cfg(target_os = "linux")]
    requests: std::sync::mpsc::Sender<(Alert, Style)>,
    /// None outside an app bundle.
    #[cfg(target_os = "macos")]
    center: Option<macos::Center>,
}

impl Notifier {
    /// Starts showing notifications. `opened` runs on Slint's event loop with the
    /// conversation of a notification someone clicked.
    pub fn start(opened: fn(String)) -> Self {
        #[cfg(target_os = "linux")]
        {
            let (requests, alerts) = std::sync::mpsc::channel();
            let started = std::thread::Builder::new()
                .name("yapfast-notify".into())
                .spawn(move || linux::run(alerts, opened));
            if let Err(error) = started {
                tracing::warn!(%error, "could not start desktop notifications");
            }
            Self { requests }
        }
        #[cfg(target_os = "macos")]
        {
            Self {
                center: macos::Center::start(opened),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = opened;
            Self {}
        }
    }

    pub fn show(&self, alert: Alert, style: Style) {
        #[cfg(target_os = "linux")]
        let _ = self.requests.send((alert, style));
        #[cfg(target_os = "macos")]
        if let Some(center) = &self.center {
            center.show(&alert, style);
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let _ = (alert, style);
    }
}

/// The body of a notification: the message, or for privacy only that there is one.
/// A call's says only that it's ringing.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn body(alert: &Alert, style: Style) -> String {
    if style.text || alert.call {
        alert.body.clone()
    } else {
        "New message".into()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex, mpsc::Receiver},
    };

    use yapfast_core::chat::Alert;
    use zbus::{
        blocking::{Connection, MessageIterator},
        zvariant::{StructureBuilder, Value},
    };

    use super::Style;

    const SERVICE: &str = "org.freedesktop.Notifications";
    const PATH: &str = "/org/freedesktop/Notifications";

    /// Shows each alert until the window closes, and opens the conversation of one
    /// that's clicked.
    pub fn run(alerts: Receiver<(Alert, Style)>, opened: fn(String)) {
        let connection = match Connection::session() {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(%error, "no session bus for desktop notifications");
                return;
            }
        };
        let capabilities: Vec<String> = connection
            .call_method(Some(SERVICE), PATH, Some(SERVICE), "GetCapabilities", &())
            .and_then(|reply| reply.body().deserialize())
            .unwrap_or_default();
        let markup = capabilities
            .iter()
            .any(|capability| capability == "body-markup");
        // Which conversation each notification shown is from, by its id.
        let shown = Arc::new(Mutex::new(HashMap::<u32, String>::new()));
        let clicks = connection.clone();
        let rooms = shown.clone();
        let listening = std::thread::Builder::new()
            .name("yapfast-notify-clicks".into())
            .spawn(move || listen(&clicks, &rooms, opened));
        if let Err(error) = listening {
            tracing::warn!(%error, "notifications can't open their conversations");
        }
        for (alert, style) in alerts {
            let mut body = super::body(&alert, style);
            if markup {
                body = escape(&body);
            }
            let mut hints = HashMap::<&str, Value>::new();
            hints.insert("category", Value::from("im.received"));
            // Yapfast plays its own sounds, and rings for a call itself.
            hints.insert("suppress-sound", Value::from(true));
            // The server finds Yapfast's name and icon through its desktop entry.
            hints.insert("desktop-entry", Value::from(crate::icon::APP_ID));
            if let Some(picture) = &alert.picture
                && let Some(image) = image_data(picture)
            {
                hints.insert("image-data", image);
            }
            let actions = ["default", "Open"];
            let request = (
                "Yapfast",
                0u32,
                "",
                alert.title.as_str(),
                body.as_str(),
                &actions[..],
                hints,
                -1i32,
            );
            let id = connection
                .call_method(Some(SERVICE), PATH, Some(SERVICE), "Notify", &request)
                .and_then(|reply| reply.body().deserialize::<u32>());
            match id {
                Ok(id) => {
                    shown
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(id, alert.room);
                }
                Err(error) => tracing::warn!(%error, "could not show a notification"),
            }
        }
    }

    /// Opens the conversation of a notification that's clicked, and forgets those closed.
    fn listen(connection: &Connection, shown: &Mutex<HashMap<u32, String>>, opened: fn(String)) {
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(SERVICE)
            .and_then(|rule| rule.path(PATH))
            .map(|rule| rule.build());
        let messages =
            rule.and_then(|rule| MessageIterator::for_match_rule(rule, connection, Some(64)));
        let messages = match messages {
            Ok(messages) => messages,
            Err(error) => {
                tracing::warn!(%error, "notifications can't open their conversations");
                return;
            }
        };
        for message in messages.flatten() {
            let header = message.header();
            let shown = || {
                shown
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
            };
            match header.member().map(|member| member.as_str()) {
                Some("ActionInvoked") => {
                    let Ok((id, action)) = message.body().deserialize::<(u32, String)>() else {
                        continue;
                    };
                    let room = shown().get(&id).cloned();
                    if let (Some(room), "default") = (room, action.as_str()) {
                        let _ = slint::invoke_from_event_loop(move || opened(room));
                    }
                }
                Some("NotificationClosed") => {
                    if let Ok((id, _reason)) = message.body().deserialize::<(u32, u32)>() {
                        shown().remove(&id);
                    }
                }
                _ => {}
            }
        }
    }

    /// The author's picture, as the specification's `(iiibiiay)` image.
    fn image_data(picture: &yapfast_core::chat::Picture) -> Option<Value<'static>> {
        let width = i32::try_from(picture.width).ok()?;
        let height = i32::try_from(picture.height).ok()?;
        let structure = StructureBuilder::new()
            .add_field(width)
            .add_field(height)
            .add_field(width.checked_mul(4)?)
            .add_field(true)
            .add_field(8i32)
            .add_field(4i32)
            .add_field(picture.rgba.to_vec())
            .build()
            .ok()?;
        Some(Value::from(structure))
    }

    /// Text for a service that reads the body as markup.
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        cell::RefCell,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU32, Ordering},
        },
    };

    use block2::{DynBlock, RcBlock};
    use image::ImageEncoder;
    use objc2::{
        AnyThread, DefinedClass, define_class, msg_send,
        rc::Retained,
        runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject},
    };
    use objc2_foundation::{NSArray, NSError, NSString, NSURL, ns_string};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
        UNNotificationAttachment, UNNotificationDefaultActionIdentifier,
        UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
        UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };
    use yapfast_core::chat::{Alert, Picture};

    use super::Style;

    pub struct Ivars {
        opened: fn(String),
    }

    define_class!(
        /// Shows notifications while Yapfast is in front too, and opens the
        /// conversation of one that's clicked. The framework may call it on any
        /// thread.
        // SAFETY: NSObject has no subclassing requirements, and `Delegate`
        // doesn't implement Drop.
        #[unsafe(super(NSObject))]
        #[name = "YapfastNotificationDelegate"]
        #[ivars = Ivars]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            /// Yapfast shows a notification only while its window isn't in
            /// front, so one arriving while the app is active still shows; and
            /// silently, as Yapfast plays its own sounds.
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                done: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                done.call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List,));
            }

            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(
                &self,
                _center: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                done: &DynBlock<dyn Fn()>,
            ) {
                // SAFETY: a constant of the framework's.
                let clicked = unsafe { UNNotificationDefaultActionIdentifier };
                if response.actionIdentifier().isEqualToString(clicked) {
                    let room = response.notification().request().identifier().to_string();
                    let opened = self.ivars().opened;
                    let _ = slint::invoke_from_event_loop(move || opened(room));
                }
                done.call(());
            }
        }
    );

    pub struct Center {
        center: Retained<UNUserNotificationCenter>,
        permission: Arc<Mutex<Permission>>,
        /// The center holds its delegate weakly.
        _delegate: Retained<Delegate>,
    }

    /// Whether Yapfast asked to show notifications. While the person hasn't
    /// answered, the framework refuses others at once, so they wait here.
    enum Permission {
        Unasked,
        Asking(Vec<Request>),
        Asked,
    }

    struct Request {
        request: Retained<UNNotificationRequest>,
        /// The author's picture's file, until the framework takes it.
        picture: Option<PathBuf>,
    }

    // SAFETY: a notification request is immutable, and the framework passes
    // them between threads itself; its answers come on a queue of its own.
    unsafe impl Send for Request {}

    impl Center {
        /// Takes clicks from now on, which a click that started Yapfast needs
        /// before the app finishes launching, as Slint's event loop starts.
        /// Asks for permission only with the first notification.
        pub fn start(opened: fn(String)) -> Option<Self> {
            // The framework raises an exception for a process without a bundle.
            if !crate::icon::bundled() {
                tracing::info!("not started as Yapfast.app; no desktop notifications");
                return None;
            }
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let delegate = Delegate::alloc().set_ivars(Ivars { opened });
            // SAFETY: NSObject's init, on a freshly allocated object.
            let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            Some(Self {
                center,
                permission: Arc::new(Mutex::new(Permission::Unasked)),
                _delegate: delegate,
            })
        }

        /// Shows the alert in place of the conversation's last, once Yapfast may.
        pub fn show(&self, alert: &Alert, style: Style) {
            let content = UNMutableNotificationContent::new();
            content.setTitle(&NSString::from_str(&alert.title));
            content.setBody(&NSString::from_str(&super::body(alert, style)));
            let room = NSString::from_str(&alert.room);
            content.setThreadIdentifier(&room);
            // No sound: Yapfast plays its own, and rings for a call itself.
            let picture = alert
                .picture
                .as_deref()
                .and_then(|picture| attach(&content, picture));
            let request = Request {
                request: UNNotificationRequest::requestWithIdentifier_content_trigger(
                    &room, &content, None,
                ),
                picture,
            };
            let mut permission = lock(&self.permission);
            match &mut *permission {
                Permission::Asking(waiting) => {
                    waiting.retain(|waiting| {
                        let replaced = waiting.request.identifier().isEqualToString(&room);
                        if replaced {
                            forget(waiting.picture.as_ref());
                        }
                        !replaced
                    });
                    waiting.push(request);
                }
                Permission::Unasked => {
                    *permission = Permission::Asking(vec![request]);
                    drop(permission);
                    self.ask(None);
                }
                Permission::Asked => {
                    drop(permission);
                    self.ask(Some(request));
                }
            }
        }

        /// Asks whether Yapfast may show notifications, then shows `request`,
        /// or with none those that waited for the first answer. Once answered,
        /// the framework answers at once with the person's choice, or what they
        /// set since in System Settings.
        fn ask(&self, request: Option<Request>) {
            let center = self.center.clone();
            let permission = self.permission.clone();
            let request = RefCell::new(request);
            let answered = RcBlock::new(move |allowed: Bool, error: *mut NSError| {
                let allowed = allowed.as_bool();
                // SAFETY: the framework passes an error or null.
                let error =
                    unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string());
                let requests = match request.borrow_mut().take() {
                    Some(request) => vec![request],
                    None => {
                        let mut permission = lock(&permission);
                        match std::mem::replace(&mut *permission, Permission::Asked) {
                            Permission::Asking(waiting) => {
                                tracing::info!(allowed, ?error, "notification permission");
                                waiting
                            }
                            other => {
                                *permission = other;
                                Vec::new()
                            }
                        }
                    }
                };
                for request in requests {
                    if allowed {
                        add(&center, request);
                    } else {
                        forget(request.picture.as_ref());
                    }
                }
            });
            self.center
                .requestAuthorizationWithOptions_completionHandler(
                    UNAuthorizationOptions::Alert,
                    &answered,
                );
        }
    }

    fn add(center: &UNUserNotificationCenter, request: Request) {
        let picture = request.picture;
        let added = RcBlock::new(move |error: *mut NSError| {
            // SAFETY: the framework passes an error or null.
            if let Some(error) = unsafe { error.as_ref() } {
                let error = error.localizedDescription().to_string();
                tracing::warn!(%error, "could not show a notification");
            }
            // The framework moved it into its own store if it took it.
            forget(picture.as_ref());
        });
        center.addNotificationRequest_withCompletionHandler(&request.request, Some(&added));
    }

    fn lock(permission: &Mutex<Permission>) -> std::sync::MutexGuard<'_, Permission> {
        permission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The author's picture, beside the text. The framework takes it as a file.
    fn attach(content: &UNMutableNotificationContent, picture: &Picture) -> Option<PathBuf> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        if u64::try_from(picture.rgba.len()).ok()?
            != u64::from(picture.width) * u64::from(picture.height) * 4
        {
            return None;
        }
        let path = std::env::temp_dir().join(format!(
            "yapfast-{}-{}.png",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let written = std::fs::File::create(&path)
            .map_err(image::ImageError::IoError)
            .and_then(|file| {
                image::codecs::png::PngEncoder::new(std::io::BufWriter::new(file)).write_image(
                    &picture.rgba,
                    picture.width,
                    picture.height,
                    image::ExtendedColorType::Rgba8,
                )
            });
        let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str()?));
        let attachment = written.map_err(|error| error.to_string()).and_then(|()| {
            // SAFETY: no options.
            unsafe {
                UNNotificationAttachment::attachmentWithIdentifier_URL_options_error(
                    ns_string!("picture"),
                    &url,
                    None,
                )
            }
            .map_err(|error| error.localizedDescription().to_string())
        });
        match attachment {
            Ok(attachment) => {
                content.setAttachments(&NSArray::from_retained_slice(&[attachment]));
                Some(path)
            }
            Err(error) => {
                tracing::debug!(%error, "a notification goes without its picture");
                forget(Some(&path));
                None
            }
        }
    }

    /// Removes a picture's file, if the framework didn't take it.
    fn forget(picture: Option<&PathBuf>) {
        if let Some(picture) = picture {
            let _ = std::fs::remove_file(picture);
        }
    }
}
