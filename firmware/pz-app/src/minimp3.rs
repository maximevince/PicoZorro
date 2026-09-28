//! The vendored minimp3 (`vendor/minimp3`, CC0), built by build.rs for the
//! MPEG features: MPEG-1/2/2.5 layers I, II and III, 16-bit output.

use core::mem::MaybeUninit;

/// Samples of one frame, all channels (1152 x 2).
pub const MAX_SAMPLES: usize = 1152 * 2;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct FrameInfo {
    pub frame_bytes: i32,
    pub frame_offset: i32,
    pub channels: i32,
    pub hz: i32,
    pub layer: i32,
    pub bitrate_kbps: i32,
}

#[repr(C)]
struct Mp3Dec {
    mdct_overlap: [[f32; 9 * 32]; 2],
    qmf_state: [f32; 15 * 2 * 32],
    reserv: i32,
    free_format_bytes: i32,
    header: [u8; 4],
    reserv_buf: [u8; 511],
}

extern "C" {
    fn mp3dec_init(dec: *mut Mp3Dec);
    fn mp3dec_decode_frame(dec: *mut Mp3Dec, mp3: *const u8, mp3_bytes: i32, pcm: *mut i16, info: *mut FrameInfo) -> i32;
}

pub struct Decoder(Mp3Dec);

impl Decoder {
    pub fn new() -> Self {
        let mut d = MaybeUninit::<Mp3Dec>::uninit();
        // SAFETY: mp3dec_init writes the fields the decoder reads before use.
        unsafe {
            mp3dec_init(d.as_mut_ptr());
            Self(d.assume_init())
        }
    }

    /// Forget the stream (after a seek): the next frame starts clean.
    pub fn reset(&mut self) {
        // SAFETY: a valid, exclusively borrowed decoder.
        unsafe { mp3dec_init(&mut self.0) }
    }

    /// One frame from the start of `mp3`: the samples per channel it
    /// produced (0: skipped or not enough data) and what minimp3 reports.
    /// `info.frame_bytes` is how much of `mp3` it used; 0 = no frame found
    /// (needs more data).
    pub fn decode(&mut self, mp3: &[u8], pcm: &mut [i16; MAX_SAMPLES]) -> (usize, FrameInfo) {
        let mut info = FrameInfo::default();
        let len = mp3.len().min(i32::MAX as usize) as i32;
        // SAFETY: pointers from live slices; pcm holds MINIMP3_MAX_SAMPLES_PER_FRAME.
        let n = unsafe { mp3dec_decode_frame(&mut self.0, mp3.as_ptr(), len, pcm.as_mut_ptr(), &mut info) };
        (n.max(0) as usize, info)
    }
}
