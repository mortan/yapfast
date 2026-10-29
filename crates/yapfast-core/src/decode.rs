//! Decoding a watched Go Live stream: VP8 with libvpx and H264 with FFmpeg,
//! both inside libwebrtc, through native/decode.c; and H265 with the HEVC
//! decoder of vendor/ffmpeg-src's FFmpeg, through native/hevc.c, where it's
//! built. On macOS, VideoToolbox decodes H264 and H265 (native/vtdec.c).

use std::ffi::{c_int, c_uchar, c_uint};

use anyhow::{Result, bail};
use livekit::webrtc::native::yuv_helper;
use yapfast_discord::rtp::Codec;

use crate::picture::{I420, RawPicture};

#[repr(C)]
struct Raw {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn yapfast_decoder_new(codec: c_int, threads: c_uint) -> *mut Raw;
    fn yapfast_decoder_decode(
        decoder: *mut Raw,
        data: *const c_uchar,
        size: usize,
        picture: *mut RawPicture,
    ) -> c_int;
    fn yapfast_decoder_free(decoder: *mut Raw);
}

#[cfg(all(hevcdec, not(target_os = "macos")))]
unsafe extern "C" {
    fn yapfast_hevc_new(threads: c_uint) -> *mut Raw;
    fn yapfast_hevc_decode(
        decoder: *mut Raw,
        data: *const c_uchar,
        size: usize,
        picture: *mut RawPicture,
    ) -> c_int;
    fn yapfast_hevc_free(decoder: *mut Raw);
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn vtdec_new(codec: c_int, reorder: c_int) -> *mut Raw;
    fn yapfast_vtdec_decode(
        decoder: *mut Raw,
        data: *const c_uchar,
        size: usize,
        picture: *mut RawPicture,
    ) -> c_int;
    fn vtdec_free(decoder: *mut Raw);
}

type DecodeFn = unsafe extern "C" fn(*mut Raw, *const c_uchar, usize, *mut RawPicture) -> c_int;
type FreeFn = unsafe extern "C" fn(*mut Raw);

/// The codecs a viewer tells Discord it decodes, most wanted first: H265
/// only where FFmpeg's HEVC decoder is built in, on Linux and Windows, or
/// on macOS, where VideoToolbox decodes it.
pub const DECODES: &[Codec] = if cfg!(hevcdec) {
    &[Codec::H265, Codec::H264, Codec::Vp8]
} else {
    &[Codec::H264, Codec::Vp8]
};

pub struct Decoder {
    raw: *mut Raw,
    codec: Codec,
    decode: DecodeFn,
    free: FreeFn,
}

// SAFETY: no library keeps thread-local state; one thread uses the decoder at a time.
unsafe impl Send for Decoder {}

/// A decoded picture, valid until the decoder's next frame.
pub struct Picture<'a> {
    pub width: u32,
    pub height: u32,
    planes: I420<'a>,
}

impl Decoder {
    /// A decoder on up to four threads, for a stream shown large.
    pub fn new(codec: Codec) -> Result<Self> {
        let threads = std::thread::available_parallelism().map_or(1, |cores| cores.get().min(4));
        Self::with_threads(codec, threads as u32)
    }

    /// A decoder on `threads` threads; each keeps frames of its own.
    pub fn with_threads(codec: Codec, threads: u32) -> Result<Self> {
        let threads = threads.max(1) as c_uint;
        // SAFETY: plain values; null means the codec couldn't start.
        let (raw, decode, free): (_, DecodeFn, FreeFn) = match codec {
            Codec::Vp8 => (
                unsafe { yapfast_decoder_new(0, threads) },
                yapfast_decoder_decode,
                yapfast_decoder_free,
            ),
            #[cfg(not(target_os = "macos"))]
            Codec::H264 => (
                unsafe { yapfast_decoder_new(1, threads) },
                yapfast_decoder_decode,
                yapfast_decoder_free,
            ),
            // VideoToolbox picks its threads, or the GPU's decoder, and
            // gives each picture as soon as it's decoded, as FFmpeg's low
            // delay does.
            #[cfg(target_os = "macos")]
            Codec::H264 => (unsafe { vtdec_new(0, 0) }, yapfast_vtdec_decode, vtdec_free),
            #[cfg(target_os = "macos")]
            Codec::H265 => (unsafe { vtdec_new(1, 0) }, yapfast_vtdec_decode, vtdec_free),
            #[cfg(all(hevcdec, not(target_os = "macos")))]
            Codec::H265 => (
                unsafe { yapfast_hevc_new(threads) },
                yapfast_hevc_decode,
                yapfast_hevc_free,
            ),
            // Not in DECODES, so Discord sends this viewer H264.
            #[cfg(not(hevcdec))]
            Codec::H265 => bail!("this build of Yapfast can't decode H265"),
        };
        if raw.is_null() {
            bail!("could not start the {codec:?} decoder");
        }
        Ok(Self {
            raw,
            codec,
            decode,
            free,
        })
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Decodes a whole frame. None while the decoder holds it back.
    pub fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture<'_>>> {
        let mut raw = RawPicture {
            width: 0,
            height: 0,
            y: std::ptr::null(),
            u: std::ptr::null(),
            v: std::ptr::null(),
            y_stride: 0,
            u_stride: 0,
            v_stride: 0,
        };
        // SAFETY: the frame outlives the call; the picture points into the
        // decoder, which `Picture` borrows.
        let decoded = unsafe { (self.decode)(self.raw, frame.as_ptr(), frame.len(), &mut raw) };
        match decoded {
            ..0 => bail!("could not decode a {:?} frame ({decoded})", self.codec),
            0 => Ok(None),
            _ => {
                // SAFETY: the decoder's planes hold the picture's rows of luma
                // and half as many of each chroma plane, at its strides.
                let Some(planes) = (unsafe { I420::from_raw(&raw) }) else {
                    bail!("the {:?} decoder gave an empty picture", self.codec);
                };
                Ok(Some(Picture {
                    width: planes.width,
                    height: planes.height,
                    planes,
                }))
            }
        }
    }
}

impl Picture<'_> {
    /// The picture's planes, as a feed's sink takes them.
    pub fn i420(&self) -> I420<'_> {
        self.planes
    }

    /// Writes the picture as RGBA, `width * 4` bytes a row.
    pub fn to_rgba(&self, out: &mut [u8]) {
        let I420 {
            y, u, v, strides, ..
        } = self.planes;
        // libyuv's ABGR is R, G, B, A in memory.
        yuv_helper::i420_to_abgr(
            y,
            strides[0],
            u,
            strides[1],
            v,
            strides[2],
            out,
            self.width * 4,
            self.width as i32,
            self.height as i32,
        );
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: raw came from the constructor `free` belongs with, and
        // is freed once.
        unsafe { (self.free)(self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../testing/fixtures")
                .join(name),
        )
        .unwrap()
    }

    fn check(codec: Codec, frame: &[u8]) {
        let mut decoder = Decoder::new(codec).unwrap();
        let picture = decoder
            .decode(frame)
            .unwrap()
            .expect("a picture from a keyframe");
        assert_eq!((picture.width, picture.height), (320, 180), "{codec:?}");
        let mut rgba = vec![0; 320 * 180 * 4];
        picture.to_rgba(&mut rgba);
        assert!(rgba.chunks_exact(4).all(|pixel| pixel[3] == 255));
        let colours: std::collections::HashSet<_> =
            rgba.chunks_exact(4).map(|pixel| pixel[0] / 32).collect();
        assert!(
            colours.len() > 3,
            "{codec:?} decoded to a picture, not a flat colour"
        );
        assert!(decoder.decode(b"not video").is_err() || codec != Codec::Vp8);
    }

    /// The first access unit of an Annex B recording: up to its second
    /// delimiter, whose NAL header starts with `delimiter`.
    fn first_unit(file: &[u8], delimiter: u8) -> &[u8] {
        let second = (1..file.len() - 4)
            .find(|&at| file[at..at + 5] == [0, 0, 0, 1, delimiter])
            .unwrap();
        &file[..second]
    }

    #[test]
    fn decodes_the_recorded_streams() {
        let vp8 = fixture("stream-vp8.ivf");
        let header = usize::from(u16::from_le_bytes([vp8[6], vp8[7]]));
        let size = u32::from_le_bytes(vp8[header..header + 4].try_into().unwrap()) as usize;
        check(Codec::Vp8, &vp8[header + 12..header + 12 + size]);
        check(Codec::H264, first_unit(&fixture("stream-h264.h264"), 9));
        // H265's delimiter is NAL type 35.
        if cfg!(hevcdec) {
            check(
                Codec::H265,
                first_unit(&fixture("stream-h265.h265"), 35 << 1),
            );
        }
    }

    /// What VideoToolbox is given of a stream's parameter sets.
    #[cfg(target_os = "macos")]
    mod videotoolbox {
        use super::*;

        /// A recording's access units, each as its NAL units.
        fn access_units(file: &[u8]) -> Vec<Vec<&[u8]>> {
            let mut units: Vec<Vec<&[u8]>> = vec![];
            for nal in yapfast_discord::rtp::nal_units(file) {
                // Each starts with a delimiter.
                if nal[0] & 0x1f == 9 {
                    units.push(vec![]);
                }
                units.last_mut().unwrap().push(nal);
            }
            units
        }

        fn annex_b(nals: &[impl AsRef<[u8]>]) -> Vec<u8> {
            nals.iter()
                .flat_map(|nal| [&[0, 0, 0, 1][..], nal.as_ref()].concat())
                .collect()
        }

        /// A NAL unit's bits after its header, without its emulation
        /// prevention bytes.
        fn payload(nal: &[u8]) -> Vec<bool> {
            let (mut bytes, mut zeros) = (vec![], 0);
            for &byte in &nal[1..] {
                if zeros >= 2 && byte == 3 {
                    zeros = 0;
                    continue;
                }
                zeros = if byte == 0 { zeros + 1 } else { 0 };
                bytes.push(byte);
            }
            bytes
                .iter()
                .flat_map(|byte| (0..8).rev().map(move |bit| byte >> bit & 1 == 1))
                .collect()
        }

        /// A NAL unit of `header` and `bits`, with emulation prevention
        /// bytes.
        fn escape(header: u8, bits: &[bool]) -> Vec<u8> {
            assert_eq!(bits.len() % 8, 0);
            let (mut nal, mut zeros) = (vec![header], 0);
            for chunk in bits.chunks(8) {
                let byte = chunk
                    .iter()
                    .fold(0u8, |byte, &bit| byte << 1 | u8::from(bit));
                if zeros >= 2 && byte <= 3 {
                    nal.push(3);
                    zeros = 0;
                }
                zeros = if byte == 0 { zeros + 1 } else { 0 };
                nal.push(byte);
            }
            nal
        }

        fn read_ue(bits: &[bool], at: &mut usize) -> u32 {
            let zeros = bits[*at..].iter().take_while(|bit| !**bit).count();
            let coded = bits[*at + zeros..=*at + 2 * zeros]
                .iter()
                .fold(0, |value, &bit| value << 1 | u32::from(bit));
            *at += 2 * zeros + 1;
            coded - 1
        }

        fn ue(value: u32) -> Vec<bool> {
            let coded = value + 1;
            let length = 32 - coded.leading_zeros();
            let mut bits = vec![false; length as usize - 1];
            bits.extend((0..length).rev().map(|bit| coded >> bit & 1 == 1));
            bits
        }

        /// A PPS of id 0, or a slice naming it, made to say `id`: 15 takes
        /// a byte more than 0, so the rest stays aligned.
        fn with_pps_id(nal: &[u8], id: u32) -> Vec<u8> {
            let bits = payload(nal);
            let mut at = 0;
            let mut named = vec![];
            // A slice names its first macroblock and its type first.
            if nal[0] & 0x1f != 8 {
                for _ in 0..2 {
                    named.extend(ue(read_ue(&bits, &mut at)));
                }
            }
            assert_eq!(read_ue(&bits, &mut at), 0);
            named.extend(ue(id));
            named.extend(&bits[at..]);
            escape(nal[0], &named)
        }

        /// Two PPSs, ids 0 and 15, before every picture, each first in
        /// turn, and slices that name 15: each picture decodes as it does
        /// with the one PPS, so neither PPS displaces the other, nor
        /// makes the decoder start over without its reference pictures.
        #[test]
        fn keeps_parameter_sets_by_their_ids() {
            let file = fixture("stream-h264.h264");
            let units = access_units(&file);
            let pictures = |units: &[Vec<u8>]| -> Vec<Vec<u8>> {
                let mut decoder = Decoder::new(Codec::H264).unwrap();
                let mut rgba = vec![0; 320 * 180 * 4];
                units
                    .iter()
                    .map(|unit| {
                        let picture = decoder.decode(unit).unwrap().expect("a picture");
                        picture.to_rgba(&mut rgba);
                        rgba.clone()
                    })
                    .collect()
            };
            let pps = units.iter().flatten().find(|nal| nal[0] & 0x1f == 8);
            let pps = pps.unwrap().to_vec();
            let both = [with_pps_id(&pps, 15), pps];
            let named: Vec<Vec<u8>> = units
                .iter()
                .enumerate()
                .map(|(index, unit)| {
                    let mut nals = vec![];
                    for nal in unit {
                        match nal[0] & 0x1f {
                            8 => {}
                            1 | 5 => {
                                if !nals.iter().any(|nal: &Vec<u8>| nal[0] & 0x1f == 8) {
                                    nals.push(both[index % 2].clone());
                                    nals.push(both[1 - index % 2].clone());
                                }
                                nals.push(with_pps_id(nal, 15));
                            }
                            _ => nals.push(nal.to_vec()),
                        }
                    }
                    annex_b(&nals)
                })
                .collect();
            let plain: Vec<Vec<u8>> = units.iter().map(|unit| annex_b(unit)).collect();
            let (plain, named) = (pictures(&plain), pictures(&named));
            assert_eq!(plain.len(), 30);
            assert!(plain == named, "the pictures with two PPSs differ");
        }
    }
}
