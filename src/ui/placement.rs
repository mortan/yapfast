//! Where the window was and how big, kept in the layout, so Yapfast starts as
//! it was left and the window made again from the tray comes back as it was.

use super::*;

/// The window's size and place while not maximized, and whether it was.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct Placement {
    /// Inside the frame, in physical pixels, or on macOS in points: macOS
    /// places windows in points, the same on every screen whatever its scale.
    width: u32,
    height: u32,
    /// The frame's top left corner on the desktop, or on macOS the content's.
    /// Wayland doesn't say, and on X11 a monitor gone since could leave it off
    /// screen, so only Windows and macOS, which tell where the monitors are,
    /// keep it from one start to the next.
    #[cfg_attr(not(any(windows, target_os = "macos")), serde(skip))]
    position: Option<[i32; 2]>,
    maximized: bool,
}

/// How long the window holds still before where it is gets written.
const SETTLE: Duration = Duration::from_secs(1);

/// Gives a window not yet shown the size and place, unless they no longer
/// fit the desktop's monitors.
pub(super) fn place(window: &YapfastWindow, placement: &Placement) {
    let native = window.window();
    #[cfg(not(target_os = "macos"))]
    {
        let (size, position) = platform::fit(
            slint::PhysicalSize::new(placement.width, placement.height),
            placement.position,
        );
        native.set_size(size);
        if let Some([x, y]) = position {
            native.set_position(slint::PhysicalPosition::new(x, y));
        }
    }
    // In points, which winit hands AppKit as they are: physical pixels it
    // would convert with the main screen's scale, not that of the screen
    // the window was on.
    #[cfg(target_os = "macos")]
    {
        let (size, position) = platform::fit(
            [f64::from(placement.width), f64::from(placement.height)],
            placement.position,
        );
        native.set_size(slint::LogicalSize::new(size[0] as f32, size[1] as f32));
        if let Some([x, y]) = position {
            native.set_position(slint::LogicalPosition::new(x as f32, y as f32));
        }
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
    #[cfg(not(target_os = "macos"))]
    let (size, position) = {
        let size = native.size();
        let position = native
            .with_winit_window(|winit| winit.outer_position().ok())
            .flatten()
            .map(|position| [position.x, position.y]);
        ([size.width, size.height], position)
    };
    #[cfg(target_os = "macos")]
    let Some((size, position)) = native.with_winit_window(platform::measure) else {
        return;
    };
    let maximized = native.is_maximized();
    let now = match adapter.layout.window {
        // Maximized, the window keeps the size and place it goes back to.
        Some(before) if maximized => Placement {
            maximized,
            ..before
        },
        _ => Placement {
            width: size[0],
            height: size[1],
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

#[cfg(target_os = "macos")]
mod platform {
    use objc2_app_kit::NSScreen;
    use objc2_foundation::{MainThreadMarker, NSRect};
    use slint::winit_030::winit::window::Window;

    /// How much of the window's top must show on a screen to keep it where
    /// it was: the title bar, by which it's moved, in points.
    const TITLE_BAR: f64 = 28.;

    /// A screen's whole area and the part windows go in, without the menu
    /// bar and the Dock, in points from the main screen's top left corner,
    /// as winit places windows.
    #[derive(Clone, Copy, Debug)]
    struct Screen {
        frame: Rect,
        visible: Rect,
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    }

    impl Rect {
        /// The area two rectangles share.
        fn overlap(&self, other: &Rect) -> f64 {
            let width = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
            let height = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
            width.max(0.) * height.max(0.)
        }
    }

    /// The window's size and its content's top left corner, in points.
    pub(super) fn measure(winit: &Window) -> ([u32; 2], Option<[i32; 2]>) {
        let scale = winit.scale_factor();
        let size = winit.inner_size().to_logical::<u32>(scale);
        let position = winit
            .inner_position()
            .ok()
            .map(|position| position.to_logical::<i32>(scale))
            .map(|position| [position.x, position.y]);
        ([size.width, size.height], position)
    }

    /// The screens, the main one, which has the menu bar, first.
    fn screens() -> Vec<Screen> {
        let Some(main_thread) = MainThreadMarker::new() else {
            return Vec::new();
        };
        let screens = NSScreen::screens(main_thread).to_vec();
        // AppKit counts up from the main screen's bottom; winit down from its top.
        let Some(top) = screens.first().map(|main| {
            let frame = main.frame();
            frame.origin.y + frame.size.height
        }) else {
            return Vec::new();
        };
        let flip = |rect: NSRect| Rect {
            x: rect.origin.x,
            y: top - rect.origin.y - rect.size.height,
            width: rect.size.width,
            height: rect.size.height,
        };
        screens
            .iter()
            .map(|screen| Screen {
                frame: flip(screen.frame()),
                visible: flip(screen.visibleFrame()),
            })
            .collect()
    }

    /// Forgets the place if the title bar would show on no screen, as after
    /// one was unplugged, and shrinks the window to the room on the screen
    /// it goes on if it no longer fits there.
    pub(super) fn fit(size: [f64; 2], position: Option<[i32; 2]>) -> ([f64; 2], Option<[i32; 2]>) {
        fit_on(&screens(), size, position)
    }

    fn fit_on(
        screens: &[Screen],
        [width, height]: [f64; 2],
        position: Option<[i32; 2]>,
    ) -> ([f64; 2], Option<[i32; 2]>) {
        if screens.is_empty() {
            return ([width, height], position);
        }
        let at = |[x, y]: [i32; 2], top: f64, height: f64| Rect {
            x: f64::from(x),
            y: f64::from(y) - top,
            width,
            height,
        };
        let position = position.filter(|&corner| {
            let title_bar = at(corner, TITLE_BAR, TITLE_BAR);
            screens
                .iter()
                .any(|screen| screen.frame.overlap(&title_bar) > 0.)
        });
        // The screen most of the window is on, or without a place the main
        // one, where winit centres it.
        let screen = match position {
            Some(corner) => {
                let window = at(corner, 0., height);
                screens.iter().max_by(|a, b| {
                    a.frame
                        .overlap(&window)
                        .total_cmp(&b.frame.overlap(&window))
                })
            }
            None => screens.first(),
        };
        let Some(room) = screen.map(|screen| screen.visible) else {
            return ([width, height], position);
        };
        let size = [
            width.min(room.width),
            height.min((room.height - TITLE_BAR).max(0.)),
        ];
        (size, position)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn screen(x: f64, y: f64, width: f64, height: f64) -> Screen {
            let frame = Rect {
                x,
                y,
                width,
                height,
            };
            // The menu bar along the top, 24 points.
            let visible = Rect {
                y: y + 24.,
                height: height - 24.,
                ..frame
            };
            Screen { frame, visible }
        }

        #[test]
        fn keeps_a_place_on_a_screen_and_drops_one_on_none() {
            let laptop = screen(0., 0., 1512., 982.);
            let monitor = screen(1512., -200., 2560., 1440.);
            let both = [laptop, monitor];
            let on_monitor = Some([2000, 100]);
            assert_eq!(
                fit_on(&both, [1400., 900.], on_monitor),
                ([1400., 900.], on_monitor)
            );
            // The monitor unplugged: macOS chooses the place, on the laptop,
            // and the window shrinks to its room below the menu bar.
            assert_eq!(
                fit_on(&[laptop], [1400., 1200.], on_monitor),
                ([1400., 982. - 24. - TITLE_BAR], None)
            );
            // Only the title bar has to show.
            assert_eq!(
                fit_on(&[laptop], [800., 600.], Some([1000, 970])).1,
                Some([1000, 970])
            );
            assert_eq!(
                fit_on(&[laptop], [800., 600.], Some([-900, 100])).1,
                None,
                "off the left edge"
            );
            assert_eq!(
                fit_on(&[laptop], [800., 600.], Some([100, 1020])).1,
                None,
                "below the bottom"
            );
        }

        #[test]
        fn shrinks_to_the_screen_it_goes_on() {
            let laptop = screen(0., 0., 1512., 982.);
            let monitor = screen(1512., 0., 2560., 1440.);
            assert_eq!(
                fit_on(&[laptop, monitor], [2400., 1300.], Some([1600, 100])).0,
                [2400., 1300.]
            );
            assert_eq!(
                fit_on(&[laptop, monitor], [2000., 1300.], Some([100, 100])).0,
                [1512., 982. - 24. - TITLE_BAR],
                "mostly on the laptop"
            );
            assert_eq!(
                fit_on(&[], [2400., 1300.], Some([100, 100])),
                ([2400., 1300.], Some([100, 100])),
                "no screens known"
            );
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    /// The desktop fits the window to the screen itself.
    pub(super) fn fit(
        size: slint::PhysicalSize,
        position: Option<[i32; 2]>,
    ) -> (slint::PhysicalSize, Option<[i32; 2]>) {
        (size, position)
    }
}
