//! Softbuffer implementation using CoreGraphics.
use crate::backend_interface::*;
use crate::error::InitError;
use crate::{Rect, SoftBufferError};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{
    define_class, msg_send, sel, AllocAnyThread, ClassType, DefinedClass, MainThreadMarker,
    Message,
};
use objc2_core_foundation::{
    CFDictionary, CFNumber, CFRetained, CFString, CFType, CGPoint,
};
use objc2_core_graphics::{kCGColorSpaceSRGB, CGColorSpace};
use objc2_io_surface::{
    kIOSurfaceBytesPerElement, kIOSurfaceBytesPerRow, kIOSurfaceColorSpace, kIOSurfaceHeight,
    kIOSurfacePixelFormat, kIOSurfaceWidth, IOSurfaceLockOptions, IOSurfacePurgeabilityState,
    IOSurfaceRef,
};
use objc2_foundation::{
    ns_string, NSDictionary, NSKeyValueChangeKey, NSKeyValueChangeNewKey,
    NSKeyValueObservingOptions, NSNumber, NSObject, NSObjectNSKeyValueObserverRegistration,
    NSString, NSValue,
};
use objc2_quartz_core::{kCAGravityTopLeft, CALayer, CATransaction};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawWindowHandle};

use std::ffi::c_void;
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::ops::Deref;
use std::ptr;
use std::slice;
use std::sync::Mutex;

/// Yapfast: the most frames a window keeps, one shown and two to draw into while the window
/// server still reads the others.
const MAX_FRAMES: usize = 3;

/// Yapfast: how long the window draws nothing before the frames it doesn't show become
/// purgeable. Purged while it animates, a frame would be drawn whole the next time it is used.
const IDLE_SECONDS: f64 = 1.0;

/// Yapfast: the age of a frame more presents behind than Slint's renderer remembers what changed
/// (three), which it draws whole.
const STALE_AGE: u8 = 4;

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "SoftbufferObserver"]
    #[ivars = ObserverIvars]
    #[derive(Debug)]
    struct Observer;

    /// NSKeyValueObserving
    impl Observer {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(
            &self,
            key_path: Option<&NSString>,
            _object: Option<&AnyObject>,
            change: Option<&NSDictionary<NSKeyValueChangeKey, AnyObject>>,
            _context: *mut c_void,
        ) {
            self.update(key_path, change);
        }

        /// Yapfast: the window has drawn nothing for `IDLE_SECONDS`.
        #[unsafe(method(makeSpareFramesPurgeable))]
        fn make_spare_frames_purgeable(&self) {
            for surface in self.ivars().take_spare() {
                // SAFETY: The old state may be null.
                unsafe {
                    surface.set_purgeable(
                        IOSurfacePurgeabilityState::PurgeableVolatile.0,
                        ptr::null_mut(),
                    )
                };
            }
        }
    }
);

#[derive(Debug)]
struct ObserverIvars {
    layer: SendCALayer,
    /// Yapfast: the frames the layer doesn't show, made purgeable once the window is idle.
    spare: Mutex<SpareFrames>,
}

impl ObserverIvars {
    fn take_spare(&self) -> Vec<CFRetained<IOSurfaceRef>> {
        std::mem::take(&mut self.spare.lock().unwrap_or_else(|e| e.into_inner()).0)
    }
}

#[derive(Debug, Default)]
struct SpareFrames(Vec<CFRetained<IOSurfaceRef>>);

// SAFETY: IOSurface objects are thread safe, as for `Frame`.
unsafe impl Send for SpareFrames {}

impl Observer {
    /// Yapfast: `spare` become purgeable unless the window draws again within `IDLE_SECONDS`.
    fn after_idle(&self, spare: Vec<CFRetained<IOSurfaceRef>>) {
        self.cancel_idle();
        self.ivars().spare.lock().unwrap_or_else(|e| e.into_inner()).0 = spare;
        // SAFETY: The selector names a method of this class that takes no argument; the run loop
        // keeps the observer until the request runs or is cancelled.
        unsafe {
            let _: () = msg_send![
                self,
                performSelector: sel!(makeSpareFramesPurgeable),
                withObject: Option::<&AnyObject>::None,
                afterDelay: IDLE_SECONDS,
            ];
        }
    }

    fn cancel_idle(&self) {
        // SAFETY: Cancels only this observer's requests for that selector.
        unsafe {
            let _: () = msg_send![
                NSObject::class(),
                cancelPreviousPerformRequestsWithTarget: self,
                selector: sel!(makeSpareFramesPurgeable),
                object: Option::<&AnyObject>::None,
            ];
        }
        self.ivars().take_spare();
    }

    fn new(layer: &CALayer) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ObserverIvars {
            layer: SendCALayer(layer.retain()),
            spare: Mutex::default(),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn update(
        &self,
        key_path: Option<&NSString>,
        change: Option<&NSDictionary<NSKeyValueChangeKey, AnyObject>>,
    ) {
        let layer = &self.ivars().layer;

        let change =
            change.expect("requested a change dictionary in `addObserver`, but none was provided");
        let new = change
            .objectForKey(unsafe { NSKeyValueChangeNewKey })
            .expect("requested change dictionary did not contain `NSKeyValueChangeNewKey`");

        // NOTE: Setting these values usually causes a quarter second animation to occur, which is
        // undesirable.
        //
        // However, since we're setting them inside an observer, there already is a transaction
        // ongoing, and as such we don't need to wrap this in a `CATransaction` ourselves.

        if key_path == Some(ns_string!("contentsScale")) {
            let new = new.downcast::<NSNumber>().unwrap();
            let scale_factor = new.as_cgfloat();

            // Set the scale factor of the layer to match the root layer when it changes (e.g. if
            // moved to a different monitor, or monitor settings changed).
            layer.setContentsScale(scale_factor);
        } else if key_path == Some(ns_string!("bounds")) {
            let new = new.downcast::<NSValue>().unwrap();
            let bounds = new.get_rect().expect("new bounds value was not CGRect");

            // Set `bounds` and `position` so that the new layer is inside the superlayer.
            //
            // This differs from just setting the `bounds`, as it also takes into account any
            // translation that the superlayer may have that we'd want to preserve.
            layer.setFrame(bounds);
        } else {
            panic!("unknown observed keypath {key_path:?}");
        }
    }
}

#[derive(Debug)]
pub struct CGImpl<D, W> {
    /// Our layer.
    layer: SendCALayer,
    /// The layer that our layer was created from.
    ///
    /// Can also be retrieved from `layer.superlayer()`.
    root_layer: SendCALayer,
    observer: Retained<Observer>,
    /// Yapfast: sRGB, which Slint's Skia renders in.
    color_space: CFRetained<CGColorSpace>,
    /// Yapfast: the frames Core Animation shows straight from their IOSurface, with no copy.
    frames: Vec<Frame>,
    /// Yapfast: the frame the layer shows.
    front: Option<usize>,
    /// The width of the underlying buffer.
    width: usize,
    /// The height of the underlying buffer.
    height: usize,
    window_handle: W,
    _display: PhantomData<D>,
}

impl<D, W> Drop for CGImpl<D, W> {
    fn drop(&mut self) {
        self.observer.cancel_idle();
        // SAFETY: Registered in `new`, must be removed before the observer is deallocated.
        unsafe {
            self.root_layer
                .removeObserver_forKeyPath(&self.observer, ns_string!("contentsScale"));
            self.root_layer
                .removeObserver_forKeyPath(&self.observer, ns_string!("bounds"));
        }
    }
}

impl<D: HasDisplayHandle, W: HasWindowHandle> SurfaceInterface<D, W> for CGImpl<D, W> {
    type Context = D;
    type Buffer<'a>
        = BufferImpl<'a, D, W>
    where
        Self: 'a;

    fn new(window_src: W, _display: &D) -> Result<Self, InitError<W>> {
        // `NSView`/`UIView` can only be accessed from the main thread.
        let _mtm = MainThreadMarker::new().ok_or(SoftBufferError::PlatformError(
            Some("can only access Core Graphics handles from the main thread".to_string()),
            None,
        ))?;

        let root_layer = match window_src.window_handle()?.as_raw() {
            RawWindowHandle::AppKit(handle) => {
                // SAFETY: The pointer came from `WindowHandle`, which ensures that the
                // `AppKitWindowHandle` contains a valid pointer to an `NSView`.
                //
                // We use `NSObject` here to avoid importing `objc2-app-kit`.
                let view: &NSObject = unsafe { handle.ns_view.cast().as_ref() };

                // Force the view to become layer backed
                let _: () = unsafe { msg_send![view, setWantsLayer: Bool::YES] };

                // SAFETY: `-[NSView layer]` returns an optional `CALayer`
                let layer: Option<Retained<CALayer>> = unsafe { msg_send![view, layer] };
                layer.expect("failed making the view layer-backed")
            }
            RawWindowHandle::UiKit(handle) => {
                // SAFETY: The pointer came from `WindowHandle`, which ensures that the
                // `UiKitWindowHandle` contains a valid pointer to an `UIView`.
                //
                // We use `NSObject` here to avoid importing `objc2-ui-kit`.
                let view: &NSObject = unsafe { handle.ui_view.cast().as_ref() };

                // SAFETY: `-[UIView layer]` returns `CALayer`
                let layer: Retained<CALayer> = unsafe { msg_send![view, layer] };
                layer
            }
            _ => return Err(InitError::Unsupported(window_src)),
        };

        // Add a sublayer, to avoid interfering with the root layer, since setting the contents of
        // e.g. a view-controlled layer is brittle.
        let layer = CALayer::new();
        root_layer.addSublayer(&layer);

        // Set the anchor point and geometry. Softbuffer's uses a coordinate system with the origin
        // in the top-left corner.
        //
        // NOTE: This doesn't really matter unless we start modifying the `position` of our layer
        // ourselves, but it's nice to have in place.
        layer.setAnchorPoint(CGPoint::new(0.0, 0.0));
        layer.setGeometryFlipped(true);

        // Do not use auto-resizing mask.
        //
        // This is done to work around a bug in macOS 14 and above, where views using auto layout
        // may end up setting fractional values as the bounds, and that in turn doesn't propagate
        // properly through the auto-resizing mask and with contents gravity.
        //
        // Instead, we keep the bounds of the layer in sync with the root layer using an observer,
        // see below.
        //
        // layer.setAutoresizingMask(kCALayerHeightSizable | kCALayerWidthSizable);

        let observer = Observer::new(&layer);
        // Observe changes to the root layer's bounds and scale factor, and apply them to our layer.
        //
        // The previous implementation updated the scale factor inside `resize`, but this works
        // poorly with transactions, and is generally inefficient. Instead, we update the scale
        // factor only when needed because the super layer's scale factor changed.
        //
        // Note that inherent in this is an explicit design decision: We control the `bounds` and
        // `contentsScale` of the layer directly, and instead let the `resize` call that the user
        // controls only be the size of the underlying buffer.
        //
        // SAFETY: Observer deregistered in `Drop` before the observer object is deallocated.
        unsafe {
            root_layer.addObserver_forKeyPath_options_context(
                &observer,
                ns_string!("contentsScale"),
                NSKeyValueObservingOptions::New | NSKeyValueObservingOptions::Initial,
                ptr::null_mut(),
            );
            root_layer.addObserver_forKeyPath_options_context(
                &observer,
                ns_string!("bounds"),
                NSKeyValueObservingOptions::New | NSKeyValueObservingOptions::Initial,
                ptr::null_mut(),
            );
        }

        // Set the content so that it is placed in the top-left corner if it does not have the same
        // size as the surface itself.
        //
        // TODO(madsmtm): Consider changing this to `kCAGravityResize` to stretch the content if
        // resized to something that doesn't fit, see #177.
        layer.setContentsGravity(unsafe { kCAGravityTopLeft });

        // Yapfast: frames carry no alpha that Core Animation should blend.
        layer.setOpaque(true);

        // Initialize color space here, to reduce work later on.
        let color_space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).unwrap();

        // Grab initial width and height from the layer (whose properties have just been initialized
        // by the observer using `NSKeyValueObservingOptionInitial`).
        let size = layer.bounds().size;
        let scale_factor = layer.contentsScale();
        let width = (size.width * scale_factor) as usize;
        let height = (size.height * scale_factor) as usize;

        Ok(Self {
            layer: SendCALayer(layer),
            root_layer: SendCALayer(root_layer),
            observer,
            color_space,
            frames: Vec::new(),
            front: None,
            width,
            height,
            _display: PhantomData,
            window_handle: window_src,
        })
    }

    #[inline]
    fn window(&self) -> &W {
        &self.window_handle
    }

    fn resize(&mut self, width: NonZeroU32, height: NonZeroU32) -> Result<(), SoftBufferError> {
        let (width, height) = (width.get() as usize, height.get() as usize);
        if (width, height) != (self.width, self.height) {
            self.width = width;
            self.height = height;
            // Yapfast: frames of the old size go; the layer keeps the one it shows until the
            // next present.
            self.frames.clear();
            self.front = None;
            self.observer.cancel_idle();
        }
        Ok(())
    }

    fn buffer_mut(&mut self) -> Result<BufferImpl<'_, D, W>, SoftBufferError> {
        // Yapfast: draw into a frame the window server is done with, never the one the layer
        // shows, and make a new one only when every other frame is still in use.
        // Of those, the one shown most recently, so Slint redraws what changed over the fewest
        // presents: two frames alternate, and a third, needed only when presents come close
        // together, is drawn whole once it falls more presents behind than Slint remembers
        // (three). Taking turns kept every frame three presents behind instead, and each redrew
        // the union of three presents' changes. A frame with no contents (age 0) last.
        let free = (0..self.frames.len())
            .filter(|&index| Some(index) != self.front && !self.frames[index].surface.is_in_use())
            .min_by_key(|&index| {
                let age = self.frames[index].age;
                (age == 0, age)
            });
        let index = match free {
            Some(index) => index,
            None if self.frames.len() < MAX_FRAMES => {
                self.frames.push(Frame {
                    surface: self.new_surface()?,
                    age: 0,
                });
                self.frames.len() - 1
            }
            None => (0..self.frames.len())
                .filter(|&index| Some(index) != self.front)
                .max_by_key(|&index| self.frames[index].age)
                .unwrap(),
        };

        let frame = &mut self.frames[index];
        let mut old_state = 0;
        // SAFETY: `old_state` is a valid pointer.
        unsafe {
            frame
                .surface
                .set_purgeable(IOSurfacePurgeabilityState::PurgeableNonVolatile.0, &mut old_state)
        };
        if old_state == IOSurfacePurgeabilityState::PurgeableEmpty.0 {
            frame.age = 0;
        }
        // SAFETY: The seed may be null. Unlocked in `present` or when the buffer drops.
        unsafe { frame.surface.lock(IOSurfaceLockOptions::empty(), ptr::null_mut()) };
        // A row may be longer than the width, for the alignment IOSurface needs; Slint takes the
        // row length from the buffer's length.
        let len = frame.surface.bytes_per_row() / 4 * self.height;
        let age = frame.age;

        Ok(BufferImpl {
            imp: self,
            index,
            age,
            len,
            locked: true,
        })
    }
}

impl<D, W> CGImpl<D, W> {
    /// Yapfast: a frame of the buffer's size, in the pixel format softbuffer uses, `0xXXRRGGBB`.
    fn new_surface(&self) -> Result<CFRetained<IOSurfaceRef>, SoftBufferError> {
        let bytes_per_row =
            IOSurfaceRef::align_property(unsafe { kIOSurfaceBytesPerRow }, self.width * 4);
        let width = CFNumber::new_isize(self.width as isize);
        let height = CFNumber::new_isize(self.height as isize);
        let bytes_per_element = CFNumber::new_i32(4);
        let bytes_per_row = CFNumber::new_isize(bytes_per_row as isize);
        let pixel_format = CFNumber::new_i32(i32::from_be_bytes(*b"BGRA"));
        // SAFETY: The keys are IOSurface's own constants.
        let keys: [&CFString; 5] = unsafe {
            [
                kIOSurfaceWidth,
                kIOSurfaceHeight,
                kIOSurfaceBytesPerElement,
                kIOSurfaceBytesPerRow,
                kIOSurfacePixelFormat,
            ]
        };
        let values: [&CFNumber; 5] = [
            &width,
            &height,
            &bytes_per_element,
            &bytes_per_row,
            &pixel_format,
        ];
        let properties = CFDictionary::from_slices(&keys, &values);
        // SAFETY: Each property has the type IOSurface documents for it.
        let surface = unsafe { IOSurfaceRef::new(properties.as_opaque()) }.ok_or_else(|| {
            SoftBufferError::PlatformError(Some("failed to create an IOSurface".to_string()), None)
        })?;
        if let Some(color_space) = self.color_space.property_list() {
            // SAFETY: A color space's property list is what `kIOSurfaceColorSpace` takes.
            unsafe { surface.set_value(kIOSurfaceColorSpace, &color_space) };
        }
        Ok(surface)
    }
}

/// Yapfast: one frame and how many presents ago it was shown.
#[derive(Debug)]
struct Frame {
    surface: CFRetained<IOSurfaceRef>,
    /// 0 for a frame with no contents yet.
    age: u8,
}

// SAFETY: IOSurface objects are thread safe; access to the pixels goes through
// `IOSurfaceLock`, which `BufferImpl` holds while it lends them out.
unsafe impl Send for Frame {}
// SAFETY: Same as above.
unsafe impl Sync for Frame {}

#[derive(Debug)]
pub struct BufferImpl<'a, D, W> {
    imp: &'a mut CGImpl<D, W>,
    index: usize,
    age: u8,
    len: usize,
    locked: bool,
}

impl<D, W> BufferImpl<'_, D, W> {
    fn pixels_ptr(&self) -> *mut u32 {
        self.imp.frames[self.index].surface.base_address().cast().as_ptr()
    }

    fn unlock(&mut self) {
        if std::mem::take(&mut self.locked) {
            // SAFETY: Locked in `buffer_mut`; the seed may be null.
            unsafe {
                self.imp.frames[self.index]
                    .surface
                    .unlock(IOSurfaceLockOptions::empty(), ptr::null_mut())
            };
        }
    }
}

impl<D, W> Drop for BufferImpl<'_, D, W> {
    fn drop(&mut self) {
        self.unlock();
    }
}

impl<D: HasDisplayHandle, W: HasWindowHandle> BufferInterface for BufferImpl<'_, D, W> {
    fn width(&self) -> NonZeroU32 {
        NonZeroU32::new(self.imp.width as u32).unwrap()
    }

    fn height(&self) -> NonZeroU32 {
        NonZeroU32::new(self.imp.height as u32).unwrap()
    }

    #[inline]
    fn pixels(&self) -> &[u32] {
        // SAFETY: The surface stays locked, and so mapped, while the buffer lives.
        unsafe { slice::from_raw_parts(self.pixels_ptr(), self.len) }
    }

    #[inline]
    fn pixels_mut(&mut self) -> &mut [u32] {
        // SAFETY: As above, and the buffer borrows the surface mutably.
        unsafe { slice::from_raw_parts_mut(self.pixels_ptr(), self.len) }
    }

    fn age(&self) -> u8 {
        self.age
    }

    fn present(mut self) -> Result<(), SoftBufferError> {
        self.unlock();
        let index = self.index;
        let imp = &mut *self.imp;

        for (other, frame) in imp.frames.iter_mut().enumerate() {
            if other == index {
                frame.age = 1;
            } else {
                frame.age = frame.age.saturating_add(u8::from(frame.age > 0));
                // Yapfast: Slint draws a frame this far behind whole, so its contents are no
                // longer worth keeping while the window animates; `buffer_mut` takes it back.
                if frame.age == STALE_AGE {
                    // SAFETY: The old state may be null.
                    unsafe {
                        frame.surface.set_purgeable(
                            IOSurfacePurgeabilityState::PurgeableVolatile.0,
                            ptr::null_mut(),
                        )
                    };
                }
            }
        }
        imp.front = Some(index);
        // Yapfast: once the window is idle, the system may take back the frames the layer doesn't
        // show, as it does with a cache, and `buffer_mut` draws such a frame whole again.
        imp.observer.after_idle(
            imp.frames
                .iter()
                .enumerate()
                .filter(|&(other, _)| other != index)
                .map(|(_, frame)| frame.surface.clone())
                .collect(),
        );

        // The CALayer has a default action associated with a change in the layer contents, causing
        // a quarter second fade transition to happen every time a new buffer is applied. This can
        // be avoided by wrapping the operation in a transaction and disabling all actions.
        CATransaction::begin();
        CATransaction::setDisableActions(true);

        // SAFETY: Core Animation shows an `IOSurface` as the layer's contents.
        let surface: &CFType = &imp.frames[index].surface;
        unsafe {
            imp.layer
                .setContents(Some(&*(surface as *const CFType).cast::<AnyObject>()))
        };

        CATransaction::commit();
        Ok(())
    }

    fn present_with_damage(self, _damage: &[Rect]) -> Result<(), SoftBufferError> {
        self.present()
    }
}

#[derive(Debug)]
struct SendCALayer(Retained<CALayer>);

// SAFETY: CALayer is dubiously thread safe, like most things in Core Animation.
// But since we make sure to do our changes within a CATransaction, it is
// _probably_ fine for us to use CALayer from different threads.
//
// See also:
// https://developer.apple.com/documentation/quartzcore/catransaction/1448267-lock?language=objc
// https://stackoverflow.com/questions/76250226/how-to-render-content-of-calayer-on-a-background-thread
unsafe impl Send for SendCALayer {}
// SAFETY: Same as above.
unsafe impl Sync for SendCALayer {}

impl Deref for SendCALayer {
    type Target = CALayer;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
