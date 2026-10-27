//! Messages to AppKit through the Objective-C runtime, for the few calls
//! Yapfast makes itself on macOS: the pointer during a file drag
//! (`crate::drop`) and the Dock icon (`crate::icon`).

use std::ffi::{CStr, c_char, c_void};

pub type Id = *mut c_void;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    pub fn sel_registerName(name: *const c_char) -> Id;
    pub fn objc_msgSend();
    pub fn objc_retain(object: Id) -> Id;
    pub fn objc_release(object: Id);
}

/// The class of this name, or null.
pub fn class(name: &CStr) -> Id {
    // SAFETY: the name is a C string; an unknown class gives null.
    unsafe { objc_getClass(name.as_ptr()) }
}

/// Sends `receiver` the message `selector` with these arguments, each
/// written `value => Type`, and the result type after `;`: `objc_msgSend`
/// called as a function of exactly those types. On arm64 a struct result,
/// such as `NSPoint` or `NSRect`, comes back in registers through it too;
/// x86_64 would need `objc_msgSend_stret` for `NSRect`. Unsafe: the types
/// must be the ones AppKit declares for the message.
macro_rules! send {
    ($receiver:expr, $selector:literal $(, $argument:expr => $kind:ty)* ; $result:ty) => {{
        let send: unsafe extern "C" fn(
            $crate::objc::Id,
            $crate::objc::Id
            $(, $kind)*
        ) -> $result = std::mem::transmute(
            $crate::objc::objc_msgSend as unsafe extern "C" fn(),
        );
        send(
            $receiver,
            $crate::objc::sel_registerName(concat!($selector, "\0").as_ptr().cast())
            $(, $argument)*
        )
    }};
}
pub(crate) use send;
