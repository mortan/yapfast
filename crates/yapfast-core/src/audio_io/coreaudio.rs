//! Sound through Core Audio's HAL output unit (AUHAL), one per stream, on
//! the device it was opened for. Each unit runs its device's IO thread, which
//! moves samples through a lock-free ring of the stream's own size; reading
//! and writing wait on the caller's thread for room or for samples, and
//! nothing on the IO thread allocates or locks. Playing, the unit converts
//! the stream's format to the device's. Recording, it converts all but the
//! rate, so a stream at another rate than the microphone's goes through an
//! AudioConverter on the reader's thread.
//!
//! WebRTC's macOS device module lists the default device first, as
//! `default`, and the others by their `AudioObjectID` in decimal; Core Audio
//! UIDs and `name:` ids open too. A unit stays on its device when the
//! default changes, so [`endpoint`] names the device by its UID for
//! `lasting.rs` to follow, as on Windows. The frameworks are declared here
//! rather than through the objc2 bindings: a dozen C functions, with no
//! Objective-C, don't need a crate. Hearing the other applications for Go
//! Live is [`tap`]'s.

use std::{
    cell::UnsafeCell,
    ffi::{c_char, c_void},
    ptr,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};

use super::Format;
use crate::voice::NAMED;

/// What a stream without a queue of its own keeps queued.
const DEFAULT_QUEUE: Duration = Duration::from_millis(200);
/// The least a microphone's ring holds, so a reader that's a little late
/// loses nothing.
const RECORDING: Duration = Duration::from_millis(200);
/// A running unit calls back every few milliseconds; this long without a
/// call means its device went away.
const SILENCE: Duration = Duration::from_secs(2);
/// How WebRTC's device module names the default device.
const DEFAULT: &str = "default";

#[cfg(feature = "discord")]
mod tap;
#[cfg(feature = "discord")]
pub use tap::others;

/// The parts of Core Audio, AudioToolbox and CoreFoundation used here, as
/// their headers declare them.
mod sys {
    use std::ffi::{c_char, c_void};

    pub type OSStatus = i32;
    pub type AudioObjectID = u32;
    pub type AudioUnit = *mut c_void;
    pub type AudioConverterRef = *mut c_void;
    pub type CFStringRef = *const c_void;

    const fn code(name: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*name)
    }

    pub const SYSTEM_OBJECT: AudioObjectID = 1;
    pub const DEVICES: u32 = code(b"dev#");
    pub const DEFAULT_INPUT: u32 = code(b"dIn ");
    pub const DEFAULT_OUTPUT: u32 = code(b"dOut");
    pub const DEVICE_UID: u32 = code(b"uid ");
    pub const NAME: u32 = code(b"lnam");
    pub const STREAMS: u32 = code(b"stm#");
    pub const LATENCY: u32 = code(b"ltnc");
    pub const SAFETY_OFFSET: u32 = code(b"saft");
    pub const BUFFER_FRAME_SIZE: u32 = code(b"fsiz");
    pub const NOMINAL_SAMPLE_RATE: u32 = code(b"nsrt");
    pub const SCOPE_GLOBAL: u32 = code(b"glob");
    pub const SCOPE_INPUT: u32 = code(b"inpt");
    pub const SCOPE_OUTPUT: u32 = code(b"outp");

    pub const TYPE_OUTPUT: u32 = code(b"auou");
    pub const SUBTYPE_HAL_OUTPUT: u32 = code(b"ahal");
    pub const MANUFACTURER_APPLE: u32 = code(b"appl");
    pub const STREAM_FORMAT: u32 = 8;
    pub const MAXIMUM_FRAMES_PER_SLICE: u32 = 14;
    pub const SET_RENDER_CALLBACK: u32 = 23;
    pub const CURRENT_DEVICE: u32 = 2000;
    pub const ENABLE_IO: u32 = 2003;
    pub const SET_INPUT_CALLBACK: u32 = 2005;
    pub const UNIT_GLOBAL: u32 = 0;
    pub const UNIT_INPUT: u32 = 1;
    pub const UNIT_OUTPUT: u32 = 2;
    pub const OUTPUT_IS_SILENCE: u32 = 1 << 4;

    pub const LINEAR_PCM: u32 = code(b"lpcm");
    pub const FLOAT: u32 = 1;
    pub const SIGNED_INTEGER: u32 = 4;
    pub const PACKED: u32 = 8;

    pub const UTF8: u32 = 0x0800_0100;

    #[repr(C)]
    pub struct PropertyAddress {
        pub selector: u32,
        pub scope: u32,
        pub element: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct StreamDescription {
        pub sample_rate: f64,
        pub format_id: u32,
        pub format_flags: u32,
        pub bytes_per_packet: u32,
        pub frames_per_packet: u32,
        pub bytes_per_frame: u32,
        pub channels_per_frame: u32,
        pub bits_per_channel: u32,
        pub reserved: u32,
    }

    #[repr(C)]
    pub struct ComponentDescription {
        pub kind: u32,
        pub subtype: u32,
        pub manufacturer: u32,
        pub flags: u32,
        pub flags_mask: u32,
    }

    #[repr(C)]
    pub struct Buffer {
        pub channels: u32,
        pub byte_size: u32,
        pub data: *mut c_void,
    }

    /// One buffer: every stream here is interleaved.
    #[repr(C)]
    pub struct BufferList {
        pub count: u32,
        pub buffers: [Buffer; 1],
    }

    /// (data, action flags, time stamp, bus, frames, buffers)
    pub type RenderCallback = unsafe extern "C" fn(
        *mut c_void,
        *mut u32,
        *const c_void,
        u32,
        u32,
        *mut BufferList,
    ) -> OSStatus;

    #[repr(C)]
    pub struct RenderCallbackStruct {
        pub callback: RenderCallback,
        pub data: *mut c_void,
    }

    /// (converter, packets, buffers, packet descriptions, data)
    pub type ConverterInput = unsafe extern "C" fn(
        AudioConverterRef,
        *mut u32,
        *mut BufferList,
        *mut *mut c_void,
        *mut c_void,
    ) -> OSStatus;

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        pub fn AudioObjectGetPropertyDataSize(
            object: AudioObjectID,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
        ) -> OSStatus;
        pub fn AudioObjectGetPropertyData(
            object: AudioObjectID,
            address: *const PropertyAddress,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> OSStatus;
    }

    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        pub fn AudioComponentFindNext(
            after: *mut c_void,
            description: *const ComponentDescription,
        ) -> *mut c_void;
        pub fn AudioComponentInstanceNew(component: *mut c_void, unit: *mut AudioUnit) -> OSStatus;
        pub fn AudioComponentInstanceDispose(unit: AudioUnit) -> OSStatus;
        pub fn AudioUnitSetProperty(
            unit: AudioUnit,
            property: u32,
            scope: u32,
            element: u32,
            data: *const c_void,
            size: u32,
        ) -> OSStatus;
        pub fn AudioUnitGetProperty(
            unit: AudioUnit,
            property: u32,
            scope: u32,
            element: u32,
            data: *mut c_void,
            size: *mut u32,
        ) -> OSStatus;
        pub fn AudioUnitInitialize(unit: AudioUnit) -> OSStatus;
        pub fn AudioUnitUninitialize(unit: AudioUnit) -> OSStatus;
        pub fn AudioOutputUnitStart(unit: AudioUnit) -> OSStatus;
        pub fn AudioOutputUnitStop(unit: AudioUnit) -> OSStatus;
        pub fn AudioUnitRender(
            unit: AudioUnit,
            flags: *mut u32,
            time: *const c_void,
            bus: u32,
            frames: u32,
            data: *mut BufferList,
        ) -> OSStatus;
        pub fn AudioConverterNew(
            from: *const StreamDescription,
            to: *const StreamDescription,
            converter: *mut AudioConverterRef,
        ) -> OSStatus;
        pub fn AudioConverterDispose(converter: AudioConverterRef) -> OSStatus;
        pub fn AudioConverterFillComplexBuffer(
            converter: AudioConverterRef,
            input: ConverterInput,
            data: *mut c_void,
            packets: *mut u32,
            output: *mut BufferList,
            descriptions: *mut c_void,
        ) -> OSStatus;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFStringGetLength(string: CFStringRef) -> isize;
        pub fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        pub fn CFStringGetCString(
            string: CFStringRef,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
        pub fn CFRelease(object: *const c_void);
    }

    // libdispatch, part of libSystem.
    unsafe extern "C" {
        pub fn dispatch_semaphore_create(value: isize) -> *mut c_void;
        pub fn dispatch_semaphore_signal(semaphore: *mut c_void) -> isize;
        pub fn dispatch_semaphore_wait(semaphore: *mut c_void, timeout: u64) -> isize;
        pub fn dispatch_time(when: u64, delta: i64) -> u64;
        pub fn dispatch_release(object: *mut c_void);
    }
}

use sys::{AudioObjectID, OSStatus};

fn check(status: OSStatus, what: &str) -> Result<()> {
    if status == 0 {
        return Ok(());
    }
    // Most of Core Audio's errors are four characters.
    let code = status.to_be_bytes();
    if code
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        bail!("{what} ('{}')", String::from_utf8_lossy(&code))
    }
    bail!("{what} ({status})")
}

fn address(selector: u32, scope: u32) -> sys::PropertyAddress {
    sys::PropertyAddress {
        selector,
        scope,
        element: 0,
    }
}

/// A plain value an object has.
fn property<T: Default>(object: AudioObjectID, selector: u32, scope: u32) -> Option<T> {
    let mut value = T::default();
    let mut size = size_of::<T>() as u32;
    // SAFETY: the value is as large as the size says.
    let status = unsafe {
        sys::AudioObjectGetPropertyData(
            object,
            &address(selector, scope),
            0,
            ptr::null(),
            &mut size,
            (&raw mut value).cast(),
        )
    };
    (status == 0).then_some(value)
}

/// A string an object has, such as its name or UID.
fn string(object: AudioObjectID, selector: u32) -> Option<String> {
    let mut value: sys::CFStringRef = ptr::null();
    let mut size = size_of::<sys::CFStringRef>() as u32;
    // SAFETY: the property is a CFString the caller releases; it's read
    // into a buffer as large as CoreFoundation says it needs.
    unsafe {
        let status = sys::AudioObjectGetPropertyData(
            object,
            &address(selector, sys::SCOPE_GLOBAL),
            0,
            ptr::null(),
            &mut size,
            (&raw mut value).cast(),
        );
        if status != 0 || value.is_null() {
            return None;
        }
        let capacity =
            sys::CFStringGetMaximumSizeForEncoding(sys::CFStringGetLength(value), sys::UTF8) + 1;
        let mut buffer = vec![0u8; capacity.max(1) as usize];
        let read = sys::CFStringGetCString(
            value,
            buffer.as_mut_ptr().cast::<c_char>(),
            capacity,
            sys::UTF8,
        );
        sys::CFRelease(value);
        if read == 0 {
            return None;
        }
        let end = buffer.iter().position(|&byte| byte == 0)?;
        buffer.truncate(end);
        String::from_utf8(buffer).ok()
    }
}

/// A list of objects the system has, such as its devices.
fn objects(selector: u32) -> Vec<AudioObjectID> {
    let list = address(selector, sys::SCOPE_GLOBAL);
    let mut size = 0;
    // SAFETY: the list is read into a buffer as large as Core Audio says.
    unsafe {
        if sys::AudioObjectGetPropertyDataSize(sys::SYSTEM_OBJECT, &list, 0, ptr::null(), &mut size)
            != 0
        {
            return Vec::new();
        }
        let mut objects = vec![0 as AudioObjectID; size as usize / size_of::<AudioObjectID>()];
        if sys::AudioObjectGetPropertyData(
            sys::SYSTEM_OBJECT,
            &list,
            0,
            ptr::null(),
            &mut size,
            objects.as_mut_ptr().cast(),
        ) != 0
        {
            return Vec::new();
        }
        objects.truncate(size as usize / size_of::<AudioObjectID>());
        objects
    }
}

/// Every device with streams to play to, or to record from.
fn devices(output: bool) -> Vec<AudioObjectID> {
    let mut devices = objects(sys::DEVICES);
    let scope = if output {
        sys::SCOPE_OUTPUT
    } else {
        sys::SCOPE_INPUT
    };
    devices.retain(|&device| {
        let mut size = 0;
        // SAFETY: a plain read of the size of the device's stream list.
        let status = unsafe {
            sys::AudioObjectGetPropertyDataSize(
                device,
                &address(sys::STREAMS, scope),
                0,
                ptr::null(),
                &mut size,
            )
        };
        status == 0 && size > 0
    });
    devices
}

/// The device behind an id, or the default one for `None`.
fn device(id: Option<&str>, output: bool) -> Result<AudioObjectID> {
    let Some(id) = id.filter(|&id| id != DEFAULT) else {
        let selector = if output {
            sys::DEFAULT_OUTPUT
        } else {
            sys::DEFAULT_INPUT
        };
        return property::<AudioObjectID>(sys::SYSTEM_OBJECT, selector, sys::SCOPE_GLOBAL)
            .filter(|&device| device != 0)
            .context("no default device");
    };
    let mut listed = devices(output).into_iter();
    let found = if let Some(name) = id.strip_prefix(NAMED) {
        listed.find(|&device| string(device, sys::NAME).as_deref() == Some(name))
    } else if let Ok(number) = id.parse::<AudioObjectID>() {
        listed.find(|&device| device == number)
    } else {
        listed.find(|&device| string(device, sys::DEVICE_UID).as_deref() == Some(id))
    };
    found.context("device not found")
}

/// The device a stream for `id` would open now, by its UID: the device's
/// own while it's connected, and otherwise the default's, which changes
/// while streams stay where they opened. Empty if there's none.
#[cfg(feature = "discord")]
pub fn endpoint(id: Option<&str>, output: bool) -> String {
    device(id, output)
        .or_else(|_| device(None, output))
        .ok()
        .and_then(|device| string(device, sys::DEVICE_UID))
        .unwrap_or_default()
}

/// How Core Audio describes `format` at `rate`.
fn description(format: Format, rate: f64) -> sys::StreamDescription {
    let frame = format.frame_bytes() as u32;
    sys::StreamDescription {
        sample_rate: rate,
        format_id: sys::LINEAR_PCM,
        format_flags: sys::PACKED
            | if format.float {
                sys::FLOAT
            } else {
                sys::SIGNED_INTEGER
            },
        bytes_per_packet: frame,
        frames_per_packet: 1,
        bytes_per_frame: frame,
        channels_per_frame: u32::from(format.channels),
        bits_per_channel: if format.float { 32 } else { 16 },
        reserved: 0,
    }
}

/// An AUHAL unit on a device.
struct Unit {
    unit: sys::AudioUnit,
    running: bool,
}

impl Unit {
    fn new(device: AudioObjectID, input: bool) -> Result<Self> {
        let description = sys::ComponentDescription {
            kind: sys::TYPE_OUTPUT,
            subtype: sys::SUBTYPE_HAL_OUTPUT,
            manufacturer: sys::MANUFACTURER_APPLE,
            flags: 0,
            flags_mask: 0,
        };
        // SAFETY: plain calls; the unit is disposed of when dropped.
        let unit = unsafe {
            let component = sys::AudioComponentFindNext(ptr::null_mut(), &description);
            ensure!(!component.is_null(), "no HAL output unit");
            let mut unit = ptr::null_mut();
            check(
                sys::AudioComponentInstanceNew(component, &mut unit),
                "no HAL output unit",
            )?;
            Self {
                unit,
                running: false,
            }
        };
        if input {
            unit.set(sys::ENABLE_IO, sys::UNIT_INPUT, 1, &1u32)?;
            unit.set(sys::ENABLE_IO, sys::UNIT_OUTPUT, 0, &0u32)?;
        }
        unit.set(sys::CURRENT_DEVICE, sys::UNIT_GLOBAL, 0, &device)
            .context("the device refused a stream")?;
        Ok(unit)
    }

    fn set<T>(&self, property: u32, scope: u32, element: u32, value: &T) -> Result<()> {
        // SAFETY: the value is as large as the size says, and the unit
        // copies it.
        check(
            unsafe {
                sys::AudioUnitSetProperty(
                    self.unit,
                    property,
                    scope,
                    element,
                    (&raw const *value).cast(),
                    size_of::<T>() as u32,
                )
            },
            "the device refused a setting",
        )
    }

    fn get<T: Default>(&self, property: u32, scope: u32, element: u32) -> Result<T> {
        let mut value = T::default();
        let mut size = size_of::<T>() as u32;
        // SAFETY: as above.
        check(
            unsafe {
                sys::AudioUnitGetProperty(
                    self.unit,
                    property,
                    scope,
                    element,
                    (&raw mut value).cast(),
                    &mut size,
                )
            },
            "the device has no such setting",
        )?;
        Ok(value)
    }

    fn initialize(&self) -> Result<()> {
        // SAFETY: a unit with its device, format and callback set.
        check(
            unsafe { sys::AudioUnitInitialize(self.unit) },
            "the device refused the stream's format",
        )
    }

    fn start(&mut self) -> Result<()> {
        if !self.running {
            // SAFETY: an initialized unit.
            check(
                unsafe { sys::AudioOutputUnitStart(self.unit) },
                "the device wouldn't start",
            )?;
            self.running = true;
        }
        Ok(())
    }

    /// Stops the unit; its callback doesn't run once this returns.
    fn stop(&mut self) {
        if self.running {
            // SAFETY: as above.
            unsafe { sys::AudioOutputUnitStop(self.unit) };
            self.running = false;
        }
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        self.stop();
        // SAFETY: the unit is no longer running, and nothing uses it after.
        unsafe {
            sys::AudioUnitUninitialize(self.unit);
            sys::AudioComponentInstanceDispose(self.unit);
        }
    }
}

/// Bytes from one thread to another, without locks: the producer only
/// writes past `written`, the consumer only reads up to it, and each
/// counter only grows.
struct Ring {
    data: Box<[UnsafeCell<u8>]>,
    written: AtomicUsize,
    taken: AtomicUsize,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            data: (0..capacity).map(|_| UnsafeCell::new(0)).collect(),
            written: AtomicUsize::new(0),
            taken: AtomicUsize::new(0),
        }
    }

    fn capacity(&self) -> usize {
        self.data.len()
    }

    fn start(&self) -> *mut u8 {
        UnsafeCell::raw_get(self.data.as_ptr())
    }

    /// Bytes waiting.
    fn len(&self) -> usize {
        let taken = self.taken.load(Ordering::Acquire);
        self.written.load(Ordering::Acquire) - taken
    }

    /// Copies in as much of `bytes` as there's room for, and says how much;
    /// from the producer only.
    fn push(&self, bytes: &[u8]) -> usize {
        let written = self.written.load(Ordering::Relaxed);
        let taken = self.taken.load(Ordering::Acquire);
        let count = bytes.len().min(self.capacity() - (written - taken));
        let at = written % self.capacity();
        let first = count.min(self.capacity() - at);
        // SAFETY: the consumer reads none of these bytes until `written`
        // says so, and both ranges lie within the ring.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), self.start().add(at), first);
            ptr::copy_nonoverlapping(bytes.as_ptr().add(first), self.start(), count - first);
        }
        self.written.store(written + count, Ordering::Release);
        count
    }

    /// Copies out as much as is waiting, up to `into`'s length, and says
    /// how much; from the consumer only.
    fn pop(&self, into: &mut [u8]) -> usize {
        let taken = self.taken.load(Ordering::Relaxed);
        let written = self.written.load(Ordering::Acquire);
        let count = into.len().min(written - taken);
        let at = taken % self.capacity();
        let first = count.min(self.capacity() - at);
        // SAFETY: the producer writes none of these bytes until `taken`
        // says so, and both ranges lie within the ring.
        unsafe {
            ptr::copy_nonoverlapping(self.start().add(at), into.as_mut_ptr(), first);
            ptr::copy_nonoverlapping(self.start(), into.as_mut_ptr().add(first), count - first);
        }
        self.taken.store(taken + count, Ordering::Release);
        count
    }

    /// Drops everything waiting; only while the consumer isn't running.
    fn clear(&self) {
        self.taken
            .store(self.written.load(Ordering::Acquire), Ordering::Release);
    }
}

/// Wakes a waiting reader or writer from the IO thread.
struct Wake {
    semaphore: *mut c_void,
    waiting: AtomicBool,
}

impl Wake {
    fn new() -> Self {
        Self {
            // SAFETY: released when dropped.
            semaphore: unsafe { sys::dispatch_semaphore_create(0) },
            waiting: AtomicBool::new(false),
        }
    }

    /// From the IO thread, after it played or recorded.
    fn signal(&self) {
        if self.waiting.swap(false, Ordering::SeqCst) {
            // SAFETY: a live semaphore; signalling neither allocates nor
            // locks.
            unsafe { sys::dispatch_semaphore_signal(self.semaphore) };
        }
    }

    /// Waits until `ready`, or for the next call back; false if none came
    /// for [`SILENCE`].
    fn wait(&self, ready: impl Fn() -> bool) -> bool {
        self.waiting.store(true, Ordering::SeqCst);
        if ready() {
            return true;
        }
        // SAFETY: as above.
        unsafe {
            let deadline = sys::dispatch_time(0, SILENCE.as_nanos() as i64);
            sys::dispatch_semaphore_wait(self.semaphore, deadline) == 0
        }
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: nothing waits or signals any more.
        unsafe { sys::dispatch_release(self.semaphore) };
    }
}

/// What a playing unit's callback reads.
struct Playing {
    ring: Ring,
    wake: Wake,
}

/// The playing unit's IO thread asks for samples.
unsafe extern "C" fn play(
    data: *mut c_void,
    flags: *mut u32,
    _: *const c_void,
    _: u32,
    _: u32,
    list: *mut sys::BufferList,
) -> OSStatus {
    // SAFETY: `data` is the output's `Playing`, which outlives its unit,
    // and the unit lends its buffers until this returns.
    unsafe {
        let playing = &*data.cast::<Playing>();
        let list = &mut *list;
        let buffers =
            std::slice::from_raw_parts_mut(list.buffers.as_mut_ptr(), list.count as usize);
        let mut silent = true;
        for (index, buffer) in buffers.iter_mut().enumerate() {
            let into =
                std::slice::from_raw_parts_mut(buffer.data.cast::<u8>(), buffer.byte_size as usize);
            // Interleaved, there's one buffer; any other gets silence.
            let played = if index == 0 {
                playing.ring.pop(into)
            } else {
                0
            };
            into[played..].fill(0);
            silent &= played == 0;
        }
        if silent {
            *flags |= sys::OUTPUT_IS_SILENCE;
        }
        playing.wake.signal();
    }
    0
}

pub struct Output {
    // Dropped first, so the callback has stopped before `playing` goes.
    unit: Unit,
    playing: Box<Playing>,
    format: Format,
    /// From leaving the ring to being heard.
    device_latency: Duration,
}

// SAFETY: the unit and the semaphore work from any thread, and the ring is
// shared only with the unit's callback.
unsafe impl Send for Output {}

impl Output {
    pub fn open(
        speaker: Option<&str>,
        _name: &'static str,
        format: Format,
        queue: Option<Duration>,
    ) -> Result<Self> {
        let device = device(speaker, true).context("these speakers are no longer connected")?;
        let unit = Unit::new(device, false)?;
        unit.set(
            sys::STREAM_FORMAT,
            sys::UNIT_INPUT,
            0,
            &description(format, f64::from(format.rate)),
        )
        .context("the device refused the stream's format")?;
        let rate = property::<f64>(device, sys::NOMINAL_SAMPLE_RATE, sys::SCOPE_GLOBAL)
            .filter(|&rate| rate > 0.0)
            .unwrap_or(f64::from(format.rate));
        let frames = |selector| property::<u32>(device, selector, sys::SCOPE_OUTPUT).unwrap_or(0);
        let buffer = frames(sys::BUFFER_FRAME_SIZE);
        let device_latency = Duration::from_secs_f64(
            f64::from(buffer + frames(sys::LATENCY) + frames(sys::SAFETY_OFFSET)) / rate,
        );
        // At least two of the device's buffers, so each callback finds one.
        let queue = queue
            .unwrap_or(DEFAULT_QUEUE)
            .max(Duration::from_secs_f64(f64::from(buffer) * 2.0 / rate));
        let frame = format.frame_bytes();
        let playing = Box::new(Playing {
            ring: Ring::new((format.bytes(queue) / frame).max(1) * frame),
            wake: Wake::new(),
        });
        unit.set(
            sys::SET_RENDER_CALLBACK,
            sys::UNIT_INPUT,
            0,
            &sys::RenderCallbackStruct {
                callback: play,
                data: (&raw const *playing).cast_mut().cast(),
            },
        )?;
        unit.initialize()?;
        Ok(Self {
            unit,
            playing,
            format,
            device_latency,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let frame = self.format.frame_bytes();
        let mut rest = &bytes[..bytes.len() / frame * frame];
        let ring = &self.playing.ring;
        while !rest.is_empty() {
            let written = ring.push(rest);
            rest = &rest[written..];
            if written == 0 {
                // Full: it plays once it has all it may queue.
                self.unit.start()?;
                let room = || ring.len() < ring.capacity();
                ensure!(self.playing.wake.wait(room), "the speakers stopped playing");
            }
        }
        Ok(())
    }

    pub fn drain(&mut self) -> Result<()> {
        let ring = &self.playing.ring;
        if ring.len() == 0 && !self.unit.running {
            return Ok(());
        }
        self.unit.start()?;
        let deadline = Instant::now() + self.queued(ring.capacity()) + Duration::from_secs(1);
        while ring.len() > 0 {
            ensure!(Instant::now() < deadline, "the speakers stopped playing");
            ensure!(
                self.playing.wake.wait(|| ring.len() == 0),
                "the speakers stopped playing"
            );
        }
        std::thread::sleep(self.device_latency);
        self.unit.stop();
        Ok(())
    }

    pub fn flush(&mut self) {
        self.unit.stop();
        self.playing.ring.clear();
    }

    pub fn latency(&self) -> Duration {
        self.queued(self.playing.ring.len()) + self.device_latency
    }

    /// How long `bytes` of the stream play.
    fn queued(&self, bytes: usize) -> Duration {
        Duration::from_secs_f64(
            (bytes / self.format.frame_bytes()) as f64 / f64::from(self.format.rate),
        )
    }
}

/// What a recording unit's callback uses.
struct Recording {
    unit: sys::AudioUnit,
    ring: Ring,
    wake: Wake,
    /// Where the unit renders each callback's samples, before the ring.
    rendered: UnsafeCell<Box<[u8]>>,
    channels: u32,
    frame: usize,
    /// Why reading fails once the device stops calling back.
    stopped: &'static str,
}

/// The recording unit's IO thread has samples.
unsafe extern "C" fn record(
    data: *mut c_void,
    flags: *mut u32,
    time: *const c_void,
    bus: u32,
    frames: u32,
    _: *mut sys::BufferList,
) -> OSStatus {
    // SAFETY: `data` is the input's `Recording`, which outlives its unit,
    // and only this callback touches `rendered`.
    unsafe {
        let recording = &*data.cast::<Recording>();
        let rendered = &mut *recording.rendered.get();
        let length = frames as usize * recording.frame;
        if length > rendered.len() {
            recording.wake.signal();
            return -50; // paramErr
        }
        let mut list = sys::BufferList {
            count: 1,
            buffers: [sys::Buffer {
                channels: recording.channels,
                byte_size: length as u32,
                data: rendered.as_mut_ptr().cast(),
            }],
        };
        let status = sys::AudioUnitRender(recording.unit, flags, time, bus, frames, &mut list);
        if status == 0 {
            let length = (list.buffers[0].byte_size as usize).min(length);
            // A reader that's fallen a whole ring behind loses the newest.
            recording.ring.push(&rendered[..length]);
        }
        recording.wake.signal();
        status
    }
}

impl Recording {
    /// Fills `bytes` from the ring, waiting for the microphone.
    fn take(&self, bytes: &mut [u8]) -> Result<()> {
        let mut filled = 0;
        while filled < bytes.len() {
            filled += self.ring.pop(&mut bytes[filled..]);
            if filled < bytes.len() {
                self.wait()?;
            }
        }
        Ok(())
    }

    /// Waits until something's in the ring.
    fn wait(&self) -> Result<()> {
        while self.ring.len() == 0 {
            ensure!(self.wake.wait(|| self.ring.len() > 0), self.stopped);
        }
        Ok(())
    }
}

/// Converts the microphone's rate to the stream's on the reader's thread.
struct Converter {
    converter: sys::AudioConverterRef,
    /// What the converter is reading, taken from the ring.
    taken: Box<[u8]>,
}

impl Converter {
    fn new(
        from: &sys::StreamDescription,
        to: &sys::StreamDescription,
        capacity: usize,
    ) -> Result<Self> {
        let mut converter = ptr::null_mut();
        // SAFETY: plain calls; disposed of when dropped.
        check(
            unsafe { sys::AudioConverterNew(from, to, &mut converter) },
            "can't convert the sound's rate",
        )?;
        Ok(Self {
            converter,
            taken: vec![0; capacity].into_boxed_slice(),
        })
    }

    /// Fills `bytes`, of `channels` with `frame` bytes each, at the
    /// stream's rate.
    fn fill(
        &mut self,
        recording: &Recording,
        bytes: &mut [u8],
        channels: u32,
        frame: usize,
    ) -> Result<()> {
        let mut supply = Supply {
            recording,
            taken: &mut self.taken,
            stopped: false,
        };
        let mut filled = 0;
        while filled < bytes.len() {
            let rest = &mut bytes[filled..];
            let mut list = sys::BufferList {
                count: 1,
                buffers: [sys::Buffer {
                    channels,
                    byte_size: rest.len() as u32,
                    data: rest.as_mut_ptr().cast(),
                }],
            };
            let mut frames = (rest.len() / frame) as u32;
            // SAFETY: the list lends `rest`, and `supply` outlives the call.
            let status = unsafe {
                sys::AudioConverterFillComplexBuffer(
                    self.converter,
                    supply_converter,
                    (&raw mut supply).cast(),
                    &mut frames,
                    &mut list,
                    ptr::null_mut(),
                )
            };
            ensure!(!supply.stopped, recording.stopped);
            check(status, "can't convert the sound's rate")?;
            ensure!(frames > 0, recording.stopped);
            filled += frames as usize * frame;
        }
        Ok(())
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        // SAFETY: nothing uses the converter after.
        unsafe { sys::AudioConverterDispose(self.converter) };
    }
}

/// What [`supply_converter`] reads from.
struct Supply<'a> {
    recording: &'a Recording,
    taken: &'a mut [u8],
    stopped: bool,
}

/// The converter asks for the microphone's samples.
unsafe extern "C" fn supply_converter(
    _: sys::AudioConverterRef,
    packets: *mut u32,
    list: *mut sys::BufferList,
    _: *mut *mut c_void,
    data: *mut c_void,
) -> OSStatus {
    // SAFETY: `data` is the `Supply` that `Converter::fill` lends for the
    // call, and what it points the list to stays until the next ask.
    unsafe {
        let supply = &mut *data.cast::<Supply<'_>>();
        let recording = supply.recording;
        if recording.wait().is_err() {
            supply.stopped = true;
            *packets = 0;
            return 1;
        }
        let wanted = (*packets as usize * recording.frame).min(supply.taken.len());
        let taken = recording.ring.pop(&mut supply.taken[..wanted]);
        (*list).buffers[0] = sys::Buffer {
            channels: recording.channels,
            byte_size: taken as u32,
            data: supply.taken.as_mut_ptr().cast(),
        };
        *packets = (taken / recording.frame) as u32;
    }
    0
}

pub struct Input {
    // Runs until dropped, which comes first, so the callback has stopped
    // before `recording` goes.
    _unit: Unit,
    recording: Box<Recording>,
    converter: Option<Converter>,
}

// SAFETY: as for `Output`.
unsafe impl Send for Input {}

impl Input {
    pub fn open(
        microphone: Option<&str>,
        _name: &'static str,
        format: Format,
        fragment: Duration,
    ) -> Result<Self> {
        let device = device(microphone, false).context("this microphone is no longer connected")?;
        let mut unit = Unit::new(device, true)?;
        // The unit converts samples and channels, but not the rate.
        let heard: sys::StreamDescription = unit.get(sys::STREAM_FORMAT, sys::UNIT_INPUT, 1)?;
        let rate = if heard.sample_rate > 0.0 {
            heard.sample_rate
        } else {
            property::<f64>(device, sys::NOMINAL_SAMPLE_RATE, sys::SCOPE_GLOBAL)
                .context("the microphone has no rate")?
        };
        let recorded = description(format, rate);
        unit.set(sys::STREAM_FORMAT, sys::UNIT_OUTPUT, 1, &recorded)
            .context("the device refused the stream's format")?;
        let slice = unit
            .get::<u32>(sys::MAXIMUM_FRAMES_PER_SLICE, sys::UNIT_GLOBAL, 0)
            .unwrap_or(0)
            .max(4096) as usize;
        let frame = format.frame_bytes();
        let at_rate = Format {
            rate: rate.round() as u32,
            ..format
        };
        let capacity =
            (at_rate.bytes((fragment * 4).max(RECORDING)) / frame * frame).max(slice * frame * 2);
        let recording = Box::new(Recording {
            unit: unit.unit,
            ring: Ring::new(capacity),
            wake: Wake::new(),
            rendered: UnsafeCell::new(vec![0; slice * frame].into_boxed_slice()),
            channels: u32::from(format.channels),
            frame,
            stopped: "the microphone stopped",
        });
        unit.set(
            sys::SET_INPUT_CALLBACK,
            sys::UNIT_GLOBAL,
            0,
            &sys::RenderCallbackStruct {
                callback: record,
                data: (&raw const *recording).cast_mut().cast(),
            },
        )?;
        unit.initialize()?;
        let converter = (at_rate.rate != format.rate)
            .then(|| {
                Converter::new(
                    &recorded,
                    &description(format, f64::from(format.rate)),
                    capacity,
                )
            })
            .transpose()?;
        unit.start()?;
        Ok(Self {
            _unit: unit,
            recording,
            converter,
        })
    }

    pub fn read(&mut self, bytes: &mut [u8]) -> Result<()> {
        let recording = &self.recording;
        match &mut self.converter {
            None => recording.take(bytes),
            Some(converter) => {
                converter.fill(recording, bytes, recording.channels, recording.frame)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_wraps_around() {
        let ring = Ring::new(8);
        assert_eq!(ring.push(&[1, 2, 3, 4, 5, 6]), 6);
        let mut out = [0; 4];
        assert_eq!(ring.pop(&mut out), 4);
        assert_eq!(out, [1, 2, 3, 4]);
        assert_eq!(ring.push(&[7, 8, 9, 10, 11, 12, 13]), 6, "only room for 6");
        let mut out = [0; 10];
        assert_eq!(ring.pop(&mut out), 8);
        assert_eq!(out[..8], [5, 6, 7, 8, 9, 10, 11, 12]);
        ring.push(&[1, 2]);
        ring.clear();
        assert_eq!(ring.len(), 0);
    }

    /// A microphone at 16 kHz read at 48 kHz: the converter takes what the
    /// unit recorded from the ring, and keeps the tone's pitch.
    #[test]
    fn converts_the_microphones_rate() {
        let at = |rate: u32| Format {
            rate,
            channels: 1,
            float: false,
        };
        let frame = at(16_000).frame_bytes();
        let recording = Recording {
            unit: ptr::null_mut(),
            ring: Ring::new(at(16_000).bytes(Duration::from_secs(2))),
            wake: Wake::new(),
            rendered: UnsafeCell::new(Box::new([])),
            channels: 1,
            frame,
            stopped: "the microphone stopped",
        };
        let tone: Vec<u8> = (0..24_000)
            .flat_map(|at| {
                let sample = 8000.0 * (at as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin();
                (sample as i16).to_ne_bytes()
            })
            .collect();
        recording.ring.push(&tone);
        let mut converter = Converter::new(
            &description(at(16_000), 16_000.0),
            &description(at(48_000), 48_000.0),
            recording.ring.capacity(),
        )
        .unwrap();
        let mut heard = vec![0u8; at(48_000).bytes(Duration::from_secs(1))];
        for chunk in heard.chunks_mut(960) {
            converter.fill(&recording, chunk, 1, frame).unwrap();
        }
        let samples: Vec<i16> = heard
            .chunks_exact(2)
            .map(|pair| i16::from_ne_bytes([pair[0], pair[1]]))
            .collect();
        let crossings = samples
            .windows(2)
            .filter(|pair| (pair[0] < 0) != (pair[1] < 0))
            .count();
        let peak = samples.iter().map(|sample| sample.unsigned_abs()).max();
        assert!((860..900).contains(&crossings), "{crossings} crossings");
        assert!(peak > Some(7000), "peak {peak:?}");
    }
}
