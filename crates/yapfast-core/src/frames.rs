//! Video frames from the stream worker to the UI, through a memory-mapped
//! file both processes open. Three slots make a triple buffer: the worker
//! fills its back slot and swaps it into the middle; the UI swaps the middle
//! out when it's newer than what it shows. Neither waits for the other, and
//! neither reads a slot the other is writing.
//!
//! The file lives in the runtime directory, which is memory on Linux, and is
//! removed once the worker has it open, so the memory returns when both
//! processes let go. Windows won't remove a mapped file, so there it's a
//! temporary file, kept in memory while it fits, that goes once the last
//! process closes it. macOS has no runtime directory, and its temporary one is
//! on disk, so there it's a POSIX shared memory object, which is memory and
//! loses its name the same way.

#[cfg(not(target_os = "macos"))]
use std::fs::OpenOptions;
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, ensure};
use memmap2::MmapMut;

const MAGIC: u32 = u32::from_le_bytes(*b"TKFR");
const HEADER: usize = 64;
const SLOTS: u32 = 3;
/// Set beside the middle slot's index when the worker put a frame there that
/// the UI hasn't taken.
const FRESH: u32 = 4;

/// The header at the start of the file; slots follow it.
#[repr(C)]
struct Header {
    magic: u32,
    capacity: u32,
    /// The middle slot's index, with [`FRESH`].
    middle: AtomicU32,
    _reserved: u32,
    /// Each slot's frame size, as width and height in pixels.
    sizes: [[AtomicU32; 2]; SLOTS as usize],
    /// When each slot's frame shows in a video file, in milliseconds.
    times: [AtomicU64; SLOTS as usize],
}

const _: () = assert!(size_of::<Header>() <= HEADER);

/// The largest frame `width` by `height` RGBA pixels fit in.
fn capacity(width: u32, height: u32) -> usize {
    width as usize * height as usize * 4
}

struct Mapping {
    map: MmapMut,
    capacity: usize,
}

impl Mapping {
    fn open(file: &File) -> Result<Self> {
        // SAFETY: only Yapfast's own processes open the file, and they touch the
        // slots through the triple buffer, never the same slot at once.
        let map = unsafe { MmapMut::map_mut(file) }.context("could not map the video frames")?;
        ensure!(map.len() >= HEADER, "the video frames file is too short");
        let mapping = Self { capacity: 0, map };
        ensure!(mapping.header().magic == MAGIC, "not a video frames file");
        let capacity = mapping.header().capacity as usize;
        ensure!(
            mapping.map.len() >= HEADER + SLOTS as usize * capacity,
            "the video frames file is too short"
        );
        Ok(Self {
            capacity,
            ..mapping
        })
    }

    fn header(&self) -> &Header {
        // SAFETY: the map is at least HEADER bytes, page aligned, and Header
        // is plain integers.
        unsafe { &*self.map.as_ptr().cast::<Header>() }
    }

    fn slot(&self, index: u32) -> *mut u8 {
        // SAFETY: Mapping::open checked the file holds every slot.
        unsafe {
            self.map
                .as_ptr()
                .cast_mut()
                .add(HEADER + index as usize * self.capacity)
        }
    }
}

/// The UI's side: it creates the file and shows the newest frame.
pub struct Frames {
    mapping: Mapping,
    /// The slot the UI reads, while it reads it.
    front: Mutex<u32>,
    name: Mutex<Option<Name>>,
}

/// What keeps the file's name until [`Frames::unlink`].
#[cfg(not(any(windows, target_os = "macos")))]
type Name = tempfile::TempPath;

#[cfg(target_os = "macos")]
use shm::Name;

#[cfg(windows)]
struct Name {
    path: PathBuf,
    /// Opened to delete the file once every process closes it.
    _file: File,
}

#[cfg(windows)]
impl std::ops::Deref for Name {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn create_file(directory: PathBuf) -> std::io::Result<(File, Name)> {
    let file = tempfile::Builder::new()
        .prefix("yapfast-frames-")
        .tempfile_in(directory)?;
    Ok(file.into_parts())
}

#[cfg(windows)]
fn create_file(directory: PathBuf) -> std::io::Result<(File, Name)> {
    use std::{os::windows::fs::OpenOptionsExt, sync::atomic::AtomicUsize};

    const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
    const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
    static CREATED: AtomicUsize = AtomicUsize::new(0);
    loop {
        let path = directory.join(format!(
            "yapfast-frames-{}-{}",
            std::process::id(),
            CREATED.fetch_add(1, Ordering::Relaxed)
        ));
        // The worker opens it with the standard library's sharing, which lets
        // it be deleted.
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .attributes(FILE_ATTRIBUTE_TEMPORARY)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .open(&path);
        match opened {
            Ok(file) => {
                let name = file.try_clone()?;
                return Ok((file, Name { path, _file: name }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

/// Opens the file the UI created, for the worker.
#[cfg(not(target_os = "macos"))]
fn open_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

#[cfg(target_os = "macos")]
use shm::open as open_file;

/// The frames as a POSIX shared memory object, whose name passes for the
/// file's path. Its memory is anonymous, as the runtime directory's is on
/// Linux, where a file in macOS's temporary directory would belong to the disk.
#[cfg(target_os = "macos")]
mod shm {
    use std::{
        ffi::{CString, c_char, c_int, c_uint},
        fs::File,
        io,
        os::{fd::FromRawFd, unix::ffi::OsStrExt},
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
    };

    unsafe extern "C" {
        fn shm_open(name: *const c_char, flags: c_int, ...) -> c_int;
        fn shm_unlink(name: *const c_char) -> c_int;
    }

    // From macOS's <sys/fcntl.h>.
    const O_RDWR: c_int = 0x0002;
    const O_CREAT: c_int = 0x0200;
    const O_EXCL: c_int = 0x0800;

    /// The object's name, removed when this drops.
    pub struct Name(PathBuf);

    impl std::ops::Deref for Name {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Name {
        fn drop(&mut self) {
            if let Ok(name) = c_name(&self.0) {
                // SAFETY: a valid C string.
                unsafe { shm_unlink(name.as_ptr()) };
            }
        }
    }

    fn c_name(path: &Path) -> io::Result<CString> {
        Ok(CString::new(path.as_os_str().as_bytes())?)
    }

    fn open_with(path: &Path, flags: c_int) -> io::Result<File> {
        let name = c_name(path)?;
        // SAFETY: a valid C string; the mode, passed as the variadic
        // argument's promoted type, counts only when it creates the object.
        let fd = unsafe { shm_open(name.as_ptr(), flags, 0o600 as c_uint) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: shm_open just opened it, and nothing else owns it.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Creates an object of this name, as an ended Yapfast leaves one.
    #[cfg(test)]
    pub fn create_named(path: &Path) -> io::Result<File> {
        open_with(path, O_RDWR | O_CREAT | O_EXCL)
    }

    /// Creates a new object, empty until it's sized.
    pub fn create() -> io::Result<(File, Name)> {
        static CREATED: AtomicUsize = AtomicUsize::new(0);
        loop {
            // macOS takes names of up to 31 bytes.
            let path = PathBuf::from(format!(
                "/yapfast-frames-{}-{}",
                std::process::id(),
                CREATED.fetch_add(1, Ordering::Relaxed)
            ));
            match open_with(&path, O_RDWR | O_CREAT | O_EXCL) {
                Ok(file) => return Ok((file, Name(path))),
                // Left by an earlier Yapfast with this process id that ended
                // before its worker opened it, and kept in memory until
                // removed: a live process with this id is this one, which
                // never names two alike, so it goes, and the next name is
                // tried. A worker that still maps it keeps its mapping.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    drop(Name(path));
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Opens the object the UI created, by its name.
    pub fn open(path: &Path) -> io::Result<File> {
        open_with(path, O_RDWR)
    }
}

impl std::fmt::Debug for Frames {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Frames")
            .field("capacity", &self.mapping.capacity)
            .finish_non_exhaustive()
    }
}

impl Frames {
    /// Creates a file for frames up to `width` by `height` pixels.
    pub fn create(width: u32, height: u32) -> Result<Self> {
        #[cfg(not(target_os = "macos"))]
        let created = create_file(
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|directory| directory.is_dir())
                .unwrap_or_else(std::env::temp_dir),
        );
        #[cfg(target_os = "macos")]
        let created = shm::create();
        let (file, name) = created.context("could not create the video frames file")?;
        let capacity = capacity(width, height);
        file.set_len((HEADER + SLOTS as usize * capacity) as u64)
            .context("could not size the video frames file")?;
        {
            // SAFETY: the file was just created and no one else has it yet.
            let mut map =
                unsafe { MmapMut::map_mut(&file) }.context("could not map the video frames")?;
            map[..4].copy_from_slice(&MAGIC.to_ne_bytes());
            map[4..8].copy_from_slice(&u32::try_from(capacity)?.to_ne_bytes());
            // The worker starts with slot 0 and the UI with slot 2.
            map[8..12].copy_from_slice(&1u32.to_ne_bytes());
        }
        Ok(Self {
            mapping: Mapping::open(&file)?,
            front: Mutex::new(2),
            name: Mutex::new(Some(name)),
        })
    }

    /// Where the worker opens the file, until [`Self::unlink`].
    pub fn path(&self) -> Option<PathBuf> {
        self.name
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_deref()
            .map(Path::to_owned)
    }

    /// Removes the file's name once the worker has it open. On Windows the
    /// name stays until both processes close the file.
    pub fn unlink(&self) {
        self.name
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }

    /// Calls `show` with the newest frame's width, height and RGBA pixels,
    /// if one arrived since the last call.
    pub fn take<T>(&self, show: impl FnOnce(u32, u32, &[u8]) -> T) -> Option<T> {
        self.take_timed(show).map(|(shown, _)| shown)
    }

    /// As [`Self::take`], with when the frame shows in a video file.
    pub fn take_timed<T>(&self, show: impl FnOnce(u32, u32, &[u8]) -> T) -> Option<(T, u64)> {
        let header = self.mapping.header();
        if header.middle.load(Ordering::Acquire) & FRESH == 0 {
            return None;
        }
        let mut front = self
            .front
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *front = header.middle.swap(*front, Ordering::AcqRel) & !FRESH;
        let [width, height] = &header.sizes[*front as usize];
        let (width, height) = (
            width.load(Ordering::Relaxed),
            height.load(Ordering::Relaxed),
        );
        let length = capacity(width, height).min(self.mapping.capacity);
        // SAFETY: the worker no longer writes this slot: it's the UI's front
        // until the UI swaps it back.
        let pixels = unsafe { std::slice::from_raw_parts(self.mapping.slot(*front), length) };
        let time = header.times[*front as usize].load(Ordering::Relaxed);
        Some((show(width, height, pixels), time))
    }
}

/// The worker's side: it fills frames.
pub struct Writer {
    mapping: Mapping,
    back: u32,
}

impl Writer {
    pub fn open(path: &Path) -> Result<Self> {
        let file = open_file(path).context("could not open the video frames file")?;
        Ok(Self {
            mapping: Mapping::open(&file)?,
            back: 0,
        })
    }

    /// The largest frame, in pixels, that fits.
    pub fn fits(&self, width: u32, height: u32) -> bool {
        capacity(width, height) <= self.mapping.capacity
    }

    /// Writes a frame with `fill`, which gets its RGBA pixels, then shows it.
    pub fn write(&mut self, width: u32, height: u32, fill: impl FnOnce(&mut [u8])) {
        self.write_timed(width, height, 0, fill);
    }

    /// As [`Self::write`], for a frame of a video file showing at `time_ms`.
    pub fn write_timed(
        &mut self,
        width: u32,
        height: u32,
        time_ms: u64,
        fill: impl FnOnce(&mut [u8]),
    ) {
        assert!(self.fits(width, height), "a frame larger than its slot");
        // SAFETY: the back slot is the worker's alone until it's swapped in.
        let pixels = unsafe {
            std::slice::from_raw_parts_mut(self.mapping.slot(self.back), capacity(width, height))
        };
        fill(pixels);
        let header = self.mapping.header();
        let [w, h] = &header.sizes[self.back as usize];
        w.store(width, Ordering::Relaxed);
        h.store(height, Ordering::Relaxed);
        header.times[self.back as usize].store(time_ms, Ordering::Relaxed);
        self.back = header.middle.swap(self.back | FRESH, Ordering::AcqRel) & !FRESH;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether the frames' name is gone: on macOS a shared memory object's
    /// name, which no file path shows.
    fn gone(path: &Path) -> bool {
        match cfg!(target_os = "macos") {
            true => Writer::open(path).is_err(),
            false => !path.exists(),
        }
    }

    #[test]
    fn frames_pass_from_writer_to_reader() {
        let frames = Frames::create(4, 2).unwrap();
        let path = frames.path().unwrap();
        let mut writer = Writer::open(&path).unwrap();
        frames.unlink();
        assert!(frames.path().is_none());
        if cfg!(not(windows)) {
            assert!(gone(&path), "its name is gone");
        }
        assert!(frames.take(|_, _, _| ()).is_none(), "nothing yet");
        assert!(writer.fits(2, 4) && !writer.fits(5, 2));

        writer.write(2, 1, |pixels| pixels.fill(1));
        writer.write(4, 2, |pixels| pixels.fill(2));
        let newest = frames.take(|width, height, pixels| (width, height, pixels.to_vec()));
        assert_eq!(newest, Some((4, 2, vec![2; 32])), "only the newest");
        assert!(frames.take(|_, _, _| ()).is_none(), "taken once");

        // Many frames while the UI holds one: it never sees a partial frame.
        for value in 3..10 {
            writer.write(1, 1, |pixels| pixels.fill(value));
        }
        assert_eq!(
            frames.take(|_, _, pixels| pixels.to_vec()),
            Some(vec![9; 4])
        );

        // A video's frames carry their time.
        writer.write_timed(1, 1, 1234, |pixels| pixels.fill(0));
        assert_eq!(frames.take_timed(|_, _, _| ()), Some(((), 1234)));

        drop((writer, frames));
        assert!(gone(&path), "gone once both let go");
    }

    /// Frames dropped before the worker said it's ready, as when it refuses
    /// the job, take their name with them.
    #[test]
    fn dropped_frames_take_their_name() {
        let frames = Frames::create(4, 2).unwrap();
        let path = frames.path().unwrap();
        drop(frames);
        assert!(gone(&path), "dropping removes the name");
    }

    /// An object an earlier Yapfast with this process id left goes when its
    /// name comes round.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_leftover_shared_memory_object_goes() {
        let frames = Frames::create(4, 2).unwrap();
        let name = frames.path().unwrap();
        let name = name.to_str().unwrap();
        let (prefix, count) = name.rsplit_once('-').unwrap();
        let next = PathBuf::from(format!("{prefix}-{}", count.parse::<usize>().unwrap() + 1));
        // Another test may have taken that name first.
        let Ok(_left) = shm::create_named(&next) else {
            return;
        };
        let later = Frames::create(4, 2).unwrap();
        assert_ne!(later.path().as_deref(), Some(next.as_path()));
        assert!(gone(&next), "the leftover went");
    }
}
