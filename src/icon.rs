//! Yapfast's icon on its window. Linux desktops find it from the window's app ID,
//! which names the desktop entry in packaging/linux, which an AppImage installs
//! for itself (`crate::desktop_entry`); Windows shows the executable's icon
//! (build.rs) in Explorer, and the window loads the same one. On macOS the
//! Dock shows Yapfast.app's icon, or, run from outside a bundle, the one
//! [`platform`] gives it.

use slint::winit_030::winit::window::WindowAttributes;

/// The window's app ID on Linux, and the name of its desktop entry and icon.
pub const APP_ID: &str = "io.github.yapfast.Yapfast";

/// Gives each window Slint makes the icon.
pub fn attributes(attributes: WindowAttributes) -> WindowAttributes {
    platform::attributes(attributes)
}

#[cfg(target_os = "linux")]
mod platform {
    use slint::winit_030::winit::window::{Icon, WindowAttributes};

    /// X11 shows the window's own icon where the desktop entry isn't
    /// installed; Wayland takes the icon only from the entry, and ignores it.
    pub fn attributes(attributes: WindowAttributes) -> WindowAttributes {
        let icon = slint::Image::load_from_svg_data(include_bytes!("../packaging/icon.svg"))
            .ok()
            .and_then(|image| image.to_rgba8())
            .and_then(|pixels| {
                Icon::from_rgba(pixels.as_bytes().to_vec(), pixels.width(), pixels.height()).ok()
            });
        attributes.with_window_icon(icon)
    }
}

#[cfg(windows)]
mod platform {
    use slint::winit_030::winit::{
        dpi::PhysicalSize,
        platform::windows::{IconExtWindows, WindowAttributesExtWindows},
        window::{Icon, WindowAttributes},
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXICON, SM_CXSMICON, SYSTEM_METRICS_INDEX,
    };

    /// The icon in packaging/windows/yapfast.rc.
    const ICON: u16 = 1;

    /// The title bar's small icon and the taskbar's large one, each loaded at
    /// the size Windows draws it rather than scaled from one picture.
    pub fn attributes(attributes: WindowAttributes) -> WindowAttributes {
        let icon = |metric: SYSTEM_METRICS_INDEX| {
            // SAFETY: GetSystemMetrics only reads a setting.
            let side = unsafe { GetSystemMetrics(metric) } as u32;
            Icon::from_resource(ICON, Some(PhysicalSize::new(side, side))).ok()
        };
        attributes
            .with_window_icon(icon(SM_CXSMICON))
            .with_taskbar_icon(icon(SM_CXICON))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::sync::Once;

    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;
    use slint::winit_030::winit::window::WindowAttributes;

    /// The logo's share of the icon's side, as Apple's icon grid has it and
    /// scripts/make-macos-icon draws Yapfast.app's.
    const LOGO: f64 = 824. / 1024.;

    /// Run from outside a bundle, such as from `target/`, the Dock would show
    /// a terminal's icon; there AppKit gets the logo, which it draws from
    /// the SVG at whatever size the Dock asks.
    pub fn attributes(attributes: WindowAttributes) -> WindowAttributes {
        static DOCK: Once = Once::new();
        DOCK.call_once(|| {
            let bundled = std::env::current_exe().ok().is_some_and(|exe| {
                exe.parent()
                    .is_some_and(|dir| dir.ends_with("Contents/MacOS"))
            });
            if !bundled {
                set_dock_icon(&padded(include_str!("../packaging/icon.svg")));
            }
        });
        attributes
    }

    /// `svg` with its view box grown so that the logo fills [`LOGO`] of it.
    fn padded(svg: &str) -> String {
        let Some(start) = svg.find("viewBox=\"").map(|at| at + "viewBox=\"".len()) else {
            return svg.to_owned();
        };
        let Some(length) = svg[start..].find('"') else {
            return svg.to_owned();
        };
        let numbers: Vec<f64> = svg[start..start + length]
            .split_whitespace()
            .filter_map(|number| number.parse().ok())
            .collect();
        let [x, y, width, height] = numbers[..] else {
            return svg.to_owned();
        };
        let side = width.max(height) / LOGO;
        let (x, y) = (x - (side - width) / 2., y - (side - height) / 2.);
        format!(
            "{}{x} {y} {side} {side}{}",
            &svg[..start],
            &svg[start + length..]
        )
    }

    /// On the main thread, where winit makes windows.
    fn set_dock_icon(svg: &str) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let data = NSData::with_bytes(svg.as_bytes());
        let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
            tracing::warn!("AppKit could not read Yapfast's icon");
            return;
        };
        // SAFETY: an image, not None.
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn pads_the_logo_as_apples_grid_does() {
            let svg = super::padded(r#"<svg viewBox="12 12 82.4 82.4"><path/></svg>"#);
            assert_eq!(svg, r#"<svg viewBox="2 2 102.4 102.4"><path/></svg>"#);
        }
    }
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod platform {
    pub fn attributes(
        attributes: slint::winit_030::winit::window::WindowAttributes,
    ) -> slint::winit_030::winit::window::WindowAttributes {
        attributes
    }
}
