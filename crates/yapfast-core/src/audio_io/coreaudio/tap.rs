//! What every other application plays, for Go Live's sound, through a Core
//! Audio process tap (macOS 14.4 on): a stereo mixdown of every process
//! but Yapfast's own, recorded through a private aggregate device of the tap
//! alone. Its IO thread copies the tap's samples, as interleaved stereo,
//! into a lock-free ring without allocating or locking, and a thread of
//! [`others`]'s own hands them on at [`RATE`], through an AudioConverter
//! when the tap runs at another rate. Like Windows, everything comes as one
//! source. `native/tap_mac.m` makes the tap, whose description is an
//! Objective-C object.
//!
//! Yapfast's own processes are this one and, when it's a stream worker, the
//! Yapfast that started it and that one's other workers, such as one playing
//! a watched stream. A process is in Core Audio's list only once it uses
//! sound, so the tap leaves out each that joins later, as the list changes.

use std::{
    cell::UnsafeCell,
    ffi::{c_int, c_void},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

use anyhow::{Context as _, Result, anyhow, bail, ensure};

use super::{
    AudioObjectID, CONVERTED, Converter, OSStatus, RECORDING, Recording, Ring, Wake, address,
    check, description, objects, property, sys,
};
use crate::audio_io::{Format, Hear, Heard, RATE, bytes_mut};

const PROCESSES: u32 = u32::from_be_bytes(*b"prs#");
const TAP_FORMAT: u32 = u32::from_be_bytes(*b"tfmt");
const PROCESS_PID: u32 = u32::from_be_bytes(*b"ppid");
/// What `proc_pidinfo` calls `PROC_PIDT_SHORTBSDINFO`.
const SHORT_BSD_INFO: c_int = 13;
/// What `proc_pidpath` may write at most.
const PATH_SIZE: usize = 4096;
/// Stereo frames the IO thread converts at a time, on its stack.
const CHUNK: usize = 256;

/// (device, now, input, input time, output, output time, data)
type IoProc = unsafe extern "C" fn(
    AudioObjectID,
    *const c_void,
    *const sys::BufferList,
    *const c_void,
    *mut sys::BufferList,
    *const c_void,
    *mut c_void,
) -> OSStatus;

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioDeviceCreateIOProcID(
        device: AudioObjectID,
        proc_: IoProc,
        data: *mut c_void,
        id: *mut *mut c_void,
    ) -> OSStatus;
    fn AudioDeviceDestroyIOProcID(device: AudioObjectID, id: *mut c_void) -> OSStatus;
    fn AudioDeviceStart(device: AudioObjectID, id: *mut c_void) -> OSStatus;
    fn AudioDeviceStop(device: AudioObjectID, id: *mut c_void) -> OSStatus;
}

// native/tap_mac.m
unsafe extern "C" {
    fn yapfast_tap_supported() -> c_int;
    fn yapfast_tap_open(
        excluded: *const AudioObjectID,
        count: usize,
        tap: *mut AudioObjectID,
        device: *mut AudioObjectID,
    ) -> OSStatus;
    fn yapfast_tap_exclude(
        tap: AudioObjectID,
        excluded: *const AudioObjectID,
        count: usize,
    ) -> OSStatus;
    fn yapfast_tap_close(tap: AudioObjectID, device: AudioObjectID);
}

// libproc, part of libSystem.
unsafe extern "C" {
    fn proc_pidpath(pid: c_int, buffer: *mut c_void, size: u32) -> c_int;
    fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buffer: *mut c_void, size: c_int)
    -> c_int;
}

/// `struct proc_bsdshortinfo`, of which only the parent is read.
#[repr(C)]
#[derive(Default)]
struct ShortBsdInfo {
    pid: u32,
    parent: u32,
    rest: [u32; 14],
}

/// A process's executable.
fn program(pid: u32) -> Option<Vec<u8>> {
    let mut path = vec![0u8; PATH_SIZE];
    // SAFETY: the buffer is as large as the size says.
    let length = unsafe { proc_pidpath(pid as c_int, path.as_mut_ptr().cast(), PATH_SIZE as u32) };
    (length > 0).then(|| {
        path.truncate(length as usize);
        path
    })
}

/// A process's parent.
fn parent(pid: u32) -> Option<u32> {
    let mut info = ShortBsdInfo::default();
    let size = size_of::<ShortBsdInfo>() as c_int;
    // SAFETY: as above.
    let read = unsafe {
        proc_pidinfo(
            pid as c_int,
            SHORT_BSD_INFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    (read == size).then_some(info.parent)
}

/// Core Audio's objects for Yapfast's own processes that use sound now.
fn yapfast_processes() -> Vec<AudioObjectID> {
    let own = std::process::id();
    let program_of_own = program(own);
    // A stream worker is the Yapfast that started it, run again.
    let yapfast = std::os::unix::process::parent_id();
    let worker = program_of_own.is_some() && program(yapfast) == program_of_own;
    objects(PROCESSES)
        .into_iter()
        .filter(|&process| {
            let Some(pid) = property::<i32>(process, PROCESS_PID, sys::SCOPE_GLOBAL) else {
                return false;
            };
            let pid = pid as u32;
            pid == own
                || worker
                    && (pid == yapfast
                        || parent(pid) == Some(yapfast) && program(pid) == program_of_own)
        })
        .collect()
}

/// Core Audio says the list of processes using sound changed.
unsafe extern "C" fn processes_changed(
    _: AudioObjectID,
    _: u32,
    _: *const sys::PropertyAddress,
    data: *mut c_void,
) -> OSStatus {
    // SAFETY: `data` is the tap's `changed`, which outlives the listener.
    unsafe { (*data.cast::<AtomicBool>()).store(true, Ordering::Relaxed) };
    0
}

/// The aggregate device's IO thread has the tap's samples: they go into the
/// ring as interleaved stereo, whichever way the device lays them out.
unsafe extern "C" fn tapped(
    _: AudioObjectID,
    _: *const c_void,
    input: *const sys::BufferList,
    _: *const c_void,
    _: *mut sys::BufferList,
    _: *const c_void,
    data: *mut c_void,
) -> OSStatus {
    // SAFETY: `data` is the tap's `Recording`, which outlives its IO proc,
    // and the device lends its float buffers until this returns.
    unsafe {
        let recording = &*data.cast::<Recording>();
        if !input.is_null() {
            let list = &*input;
            let buffers = std::slice::from_raw_parts(list.buffers.as_ptr(), list.count as usize);
            let samples = |index: usize| {
                let buffer: &sys::Buffer = &buffers[index];
                let count = buffer.byte_size as usize / size_of::<f32>();
                let samples = if buffer.data.is_null() || count == 0 {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(buffer.data.cast::<f32>().cast_const(), count)
                };
                (samples, (buffer.channels as usize).max(1))
            };
            if !buffers.is_empty() {
                // Left is the first buffer's first channel; right its
                // second, the second buffer's first, or left again.
                let (first, stride) = samples(0);
                let (right, right_stride, right_at) = if stride >= 2 {
                    (first, stride, 1)
                } else if buffers.len() >= 2 {
                    let (second, stride) = samples(1);
                    (second, stride, 0)
                } else {
                    (first, stride, 0)
                };
                let frames = (first.len() / stride).min(right.len() / right_stride);
                let mut chunk = [0f32; CHUNK * 2];
                for start in (0..frames).step_by(CHUNK) {
                    let count = CHUNK.min(frames - start);
                    for (at, pair) in chunk[..count * 2].chunks_exact_mut(2).enumerate() {
                        let frame = start + at;
                        pair[0] = first[frame * stride];
                        pair[1] = right[frame * right_stride + right_at];
                    }
                    // A hearing thread that's fallen a whole ring behind
                    // loses the newest.
                    recording
                        .ring
                        .push(crate::audio_io::bytes(&chunk[..count * 2]));
                }
            }
        }
        recording.wake.signal();
    }
    0
}

/// What the IO thread fills: `capacity` bytes of interleaved stereo floats.
fn stereo_ring(capacity: usize) -> Recording {
    Recording {
        unit: ptr::null_mut(),
        ring: Ring::new(capacity),
        wake: Wake::new(),
        rendered: UnsafeCell::new(Box::new([])),
        channels: 2,
        frame: 2 * size_of::<f32>(),
        stopped: "the system's sound stopped",
    }
}

/// The tap and its aggregate device, destroyed when dropped.
struct Objects {
    tap: AudioObjectID,
    device: AudioObjectID,
}

impl Drop for Objects {
    fn drop(&mut self) {
        // SAFETY: objects this made, which nothing uses after.
        unsafe { yapfast_tap_close(self.tap, self.device) };
    }
}

/// A running tap, until dropped.
struct Tap {
    io: *mut c_void,
    recording: Box<Recording>,
    /// Set when the processes using sound change.
    changed: Box<AtomicBool>,
    /// The process objects left out.
    excluded: Vec<AudioObjectID>,
    converter: Option<Converter>,
    // Dropped last, once the IO proc and the listener are gone.
    objects: Objects,
}

impl Tap {
    fn open() -> Result<Self> {
        let excluded = yapfast_processes();
        let mut objects = Objects { tap: 0, device: 0 };
        // SAFETY: the list is as long as the count says.
        check(
            unsafe {
                yapfast_tap_open(
                    excluded.as_ptr(),
                    excluded.len(),
                    &mut objects.tap,
                    &mut objects.device,
                )
            },
            "can't tap the other applications",
        )?;
        let tapped_format =
            property::<sys::StreamDescription>(objects.tap, TAP_FORMAT, sys::SCOPE_GLOBAL)
                .context("the tap has no format")?;
        ensure!(
            tapped_format.format_id == sys::LINEAR_PCM
                && tapped_format.format_flags & sys::FLOAT != 0
                && tapped_format.bits_per_channel == 32,
            "the tap's samples aren't floats"
        );
        let rate = property::<f64>(objects.device, sys::NOMINAL_SAMPLE_RATE, sys::SCOPE_GLOBAL)
            .filter(|&rate| rate > 0.0)
            .unwrap_or(tapped_format.sample_rate);
        ensure!(rate > 0.0, "the tap has no rate");
        tracing::debug!(rate, excluded = ?excluded, "tapping the other applications");
        let stereo = |rate: u32| Format {
            rate,
            channels: 2,
            float: true,
        };
        let at_rate = stereo(rate.round() as u32);
        let recording = Box::new(stereo_ring(at_rate.bytes(RECORDING)));
        let converter = (at_rate.rate != RATE)
            .then(|| {
                Converter::new(
                    &description(at_rate, rate),
                    &description(stereo(RATE), f64::from(RATE)),
                    at_rate.bytes(CONVERTED),
                )
            })
            .transpose()?;
        let mut tap = Self {
            io: ptr::null_mut(),
            recording,
            changed: Box::new(AtomicBool::new(false)),
            excluded,
            converter,
            objects,
        };
        // SAFETY: the recording and `changed` outlive the IO proc and the
        // listener, which `drop` removes.
        unsafe {
            check(
                AudioDeviceCreateIOProcID(
                    tap.objects.device,
                    tapped,
                    (&raw const *tap.recording).cast_mut().cast(),
                    &mut tap.io,
                ),
                "can't record the tap",
            )?;
            // The listener is called in a stream worker, which runs no
            // main run loop, too.
            super::follow_devices();
            check(
                sys::AudioObjectAddPropertyListener(
                    sys::SYSTEM_OBJECT,
                    &address(PROCESSES, sys::SCOPE_GLOBAL),
                    processes_changed,
                    (&raw const *tap.changed).cast_mut().cast(),
                ),
                "can't follow Yapfast's processes",
            )?;
            check(
                AudioDeviceStart(tap.objects.device, tap.io),
                "can't start recording the tap",
            )?;
        }
        // Yapfast's processes that came to use sound while it opened.
        tap.changed.store(true, Ordering::Relaxed);
        Ok(tap)
    }

    /// Leaves out Yapfast's processes that came to use sound since.
    fn exclude(&mut self) -> Result<()> {
        let excluded = yapfast_processes();
        if excluded != self.excluded {
            // SAFETY: the list is as long as the count says.
            check(
                unsafe { yapfast_tap_exclude(self.objects.tap, excluded.as_ptr(), excluded.len()) },
                "can't leave Yapfast's processes out",
            )?;
            self.excluded = excluded;
        }
        Ok(())
    }

    /// Passes on what the tap hears, 10 ms at a time, until `stop`.
    fn hear(&mut self, heard: &mut dyn FnMut(Heard<'_>), stop: &AtomicBool) -> Result<()> {
        let mut samples = vec![0f32; RATE as usize / 100 * 2];
        while !stop.load(Ordering::Relaxed) {
            if self.changed.swap(false, Ordering::Relaxed) {
                self.exclude()?;
            }
            let recording = &self.recording;
            let bytes = bytes_mut(&mut samples);
            match &mut self.converter {
                None => recording.take(bytes)?,
                Some(converter) => converter.fill(recording, bytes)?,
            }
            // Nothing playing: the mixer hears nothing rather than silence.
            if samples.iter().any(|&sample| sample != 0.0) {
                heard(Heard::Samples {
                    source: 0,
                    samples: &samples,
                });
            }
        }
        Ok(())
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        // SAFETY: stopping and destroying the IO proc and the listener this
        // made; neither calls back once this returns.
        unsafe {
            if !self.io.is_null() {
                AudioDeviceStop(self.objects.device, self.io);
                AudioDeviceDestroyIOProcID(self.objects.device, self.io);
            }
            sys::AudioObjectRemovePropertyListener(
                sys::SYSTEM_OBJECT,
                &address(PROCESSES, sys::SCOPE_GLOBAL),
                processes_changed,
                (&raw const *self.changed).cast_mut().cast(),
            );
        }
    }
}

pub fn others(
    _skip: fn(&str) -> bool,
    mut heard: Hear,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    // SAFETY: a plain call.
    if unsafe { yapfast_tap_supported() } == 0 {
        bail!("sharing other applications' sound needs macOS 14.4 or later");
    }
    let (opened, opening) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("yapfast-stream-hear".into())
        .spawn(move || {
            let mut tap = match Tap::open() {
                Ok(tap) => {
                    let _ = opened.send(Ok(()));
                    tap
                }
                Err(error) => {
                    let _ = opened.send(Err(error));
                    return;
                }
            };
            heard(Heard::Started {
                source: 0,
                process: None,
            });
            if let Err(error) = tap.hear(&mut *heard, &stop) {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "stopped hearing other applications"
                );
            }
            heard(Heard::Stopped { source: 0 });
        })
        .context("could not start hearing other applications")?;
    match opening.recv() {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error.context("macOS won't share other applications' sound"))
        }
        Err(_) => {
            let _ = thread.join();
            Err(anyhow!("could not hear other applications"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whichever way the device lays out its samples, the ring gets
    /// interleaved stereo.
    #[test]
    fn the_io_thread_hands_on_stereo() {
        #[repr(C)]
        struct Two {
            count: u32,
            buffers: [sys::Buffer; 2],
        }
        let mut left = [0.1f32, 0.2, 0.3];
        let mut right = [-0.1f32, -0.2, -0.3];
        let mut interleaved = [0.5f32, -0.5, 0.6, -0.6];
        let buffer = |samples: &mut [f32], channels: u32| sys::Buffer {
            channels,
            byte_size: size_of_val(samples) as u32,
            data: samples.as_mut_ptr().cast(),
        };
        let separate = Two {
            count: 2,
            buffers: [buffer(&mut left, 1), buffer(&mut right, 1)],
        };
        let together = sys::BufferList {
            count: 1,
            buffers: [buffer(&mut interleaved, 2)],
        };
        let recording = stereo_ring(64);
        for list in [(&raw const separate).cast(), &raw const together] {
            // SAFETY: lists of live buffers, and a recording that outlives
            // the calls.
            unsafe {
                tapped(
                    0,
                    ptr::null(),
                    list,
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null(),
                    (&raw const recording).cast_mut().cast(),
                )
            };
        }
        let mut heard = [0f32; 10];
        assert_eq!(recording.ring.pop(bytes_mut(&mut heard)), 40);
        assert_eq!(
            heard,
            [0.1, -0.1, 0.2, -0.2, 0.3, -0.3, 0.5, -0.5, 0.6, -0.6]
        );
    }
}
