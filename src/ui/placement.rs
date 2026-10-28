//! Where the window was and how big, kept in the layout, so Yapfast starts as
//! it was left and the window made again from the tray comes back as it was.

use super::*;

/// The window's size and place while not maximized, and whether it was.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct Placement {
    /// Inside the frame, in physical pixels.
    width: u32,
    height: u32,
    /// The frame's top left corner on the desktop, or on macOS the content's.
    /// Wayland doesn't say, and on X11 a monitor gone since could leave it off
    /// screen, so only Windows, which tells where the monitors are, keeps it
    /// from one start to the next.
    #[cfg_attr(not(windows), serde(skip))]
    position: Option<[i32; 2]>,
    maximized: bool,
}

/// How long the window holds still before where it is gets written.
const SETTLE: Duration = Duration::from_secs(1);

/// Gives a window not yet shown the size and place, unless they no longer
/// fit the desktop's monitors.
pub(super) fn place(window: &YapfastWindow, placement: &Placement) {
    let native = window.window();
    let (size, position) = platform::fit(
        slint::PhysicalSize::new(placement.width, placement.height),
        placement.position,
    );
    native.set_size(size);
    if let Some([x, y]) = position {
        native.set_position(slint::PhysicalPosition::new(x, y));
    }
    // Maximized over the size and place above, which un-maximizing returns to.
    native.set_maximized(placement.maximized);
}

/// The window moved, resized, maximized or came back: keeps where it is now,
/// written once it holds still.
pub(super) fn moved(adapter: &mut Adapter, window: &YapfastWindow) {
    let native = window.window();
    // Minimized, Windows puts the window far off screen.
    if native.is_minimized() {
        return;
    }
    let size = native.size();
    // winit on macOS makes a window at a place by its content's corner, not
    // its frame's, so that's the corner kept there.
    let position = native
        .with_winit_window(|winit| match cfg!(target_os = "macos") {
            true => winit.inner_position().ok(),
            false => winit.outer_position().ok(),
        })
        .flatten()
        .map(|position| [position.x, position.y]);
    let maximized = native.is_maximized();
    let now = match adapter.layout.window {
        // Maximized, the window keeps the size and place it goes back to.
        Some(before) if maximized => Placement {
            maximized,
            ..before
        },
        _ => Placement {
            width: size.width,
            height: size.height,
            position,
            maximized,
        },
    };
    if adapter.layout.window == Some(now) {
        return;
    }
    adapter.layout.window = Some(now);
    let adapter_weak = adapter.weak.clone();
    adapter
        .placement_due
        .start(slint::TimerMode::SingleShot, SETTLE, move || {
            if let Some(adapter) = adapter_weak.upgrade() {
                let adapter = adapter.borrow();
                adapter.layout.write(&adapter.root);
            }
        });
}

/// Writes where the window is if it moved since the last write, as Yapfast ends.
pub(super) fn flush(adapter: &Adapter) {
    if adapter.placement_due.running() {
        adapter.placement_due.stop();
        adapter.layout.write(&adapter.root);
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::{
        Foundation::{POINT, RECT},
        Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL,
            MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, MonitorFromRect,
        },
        UI::WindowsAndMessaging::{AdjustWindowRectEx, WS_OVERLAPPEDWINDOW},
    };

    /// How much of the frame's top must show on a monitor to keep the window
    /// where it was: about the title bar, by which it's moved.
    const TITLE_BAR: u32 = 32;

    /// Forgets the place if the title bar would show on no monitor, as after
    /// one was unplugged, and shrinks the window to the work area of the
    /// monitor it goes on if it no longer fits there.
    pub(super) fn fit(
        size: slint::PhysicalSize,
        position: Option<[i32; 2]>,
    ) -> (slint::PhysicalSize, Option<[i32; 2]>) {
        let rect = |[x, y]: [i32; 2], height: u32| RECT {
            left: x,
            top: y,
            right: x.saturating_add_unsigned(size.width),
            bottom: y.saturating_add_unsigned(height),
        };
        let position = position.filter(|&at| {
            // SAFETY: only reads the rectangle passed.
            !unsafe { MonitorFromRect(&rect(at, TITLE_BAR), MONITOR_DEFAULTTONULL) }.is_null()
        });
        // SAFETY: as above. Without a place, Windows puts the window on the
        // primary monitor.
        let monitor = unsafe {
            match position {
                Some(at) => MonitorFromRect(&rect(at, size.height), MONITOR_DEFAULTTONEAREST),
                None => MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY),
            }
        };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a MONITORINFO with its size set, as the call asks.
        if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
            return (size, position);
        }
        // The size is inside the frame, so the frame comes off the work area.
        let mut frame = RECT::default();
        // SAFETY: only changes the rectangle passed.
        unsafe { AdjustWindowRectEx(&mut frame, WS_OVERLAPPEDWINDOW, 0, 0) };
        let work = info.rcWork;
        let room = |work: i32, frame: i32| (work - frame).max(0) as u32;
        let size = slint::PhysicalSize::new(
            size.width
                .min(room(work.right - work.left, frame.right - frame.left)),
            size.height
                .min(room(work.bottom - work.top, frame.bottom - frame.top)),
        );
        (size, position)
    }
}

#[cfg(not(windows))]
mod platform {
    /// The desktop fits the window to the screen itself.
    pub(super) fn fit(
        size: slint::PhysicalSize,
        position: Option<[i32; 2]>,
    ) -> (slint::PhysicalSize, Option<[i32; 2]>) {
        (size, position)
    }
}
