//! Virtual webcam: decode H.264 and write I420 frames to a `v4l2loopback`
//! output device labelled "Linker Camera". Video only — there is no audio
//! path in Linker.
//!
//! Decoder: OpenH264 (software; the Mac encodes Constrained Baseline so the
//! stream is always decodable). 720p30 costs a few percent of one core.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use linker_platform::{CameraParams, CameraSink, Cap, Unsupported};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

pub const CARD_LABEL: &str = "Linker Camera";

// --- minimal V4L2 ABI (linux/videodev2.h) ----------------------------------

const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const V4L2_FIELD_NONE: u32 = 1;
const V4L2_COLORSPACE_SRGB: u32 = 8;
const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}
const V4L2_PIX_FMT_YUV420: u32 = fourcc(b'Y', b'U', b'1', b'2');

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format`: u32 type + 200-byte union, 8-byte aligned on 64-bit.
#[repr(C)]
struct V4l2Format {
    type_: u32,
    fmt: V4l2FormatUnion,
}

#[repr(C)]
union V4l2FormatUnion {
    pix: V4l2PixFormat,
    raw: [u8; 200],
    _align: [u64; 25],
}

const fn iowr(ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    ((3u64 << 30) | ((size as u64) << 16) | ((ty as u64) << 8) | nr as u64) as libc::c_ulong
}
const VIDIOC_S_FMT: libc::c_ulong = iowr(b'V', 5, std::mem::size_of::<V4l2Format>());

fn set_output_format(f: &File, w: u32, h: u32) -> anyhow::Result<()> {
    let mut fmt = V4l2Format {
        type_: V4L2_BUF_TYPE_VIDEO_OUTPUT,
        fmt: V4l2FormatUnion { raw: [0; 200] },
    };
    fmt.fmt.pix = V4l2PixFormat {
        width: w,
        height: h,
        pixelformat: V4L2_PIX_FMT_YUV420,
        field: V4L2_FIELD_NONE,
        bytesperline: w,
        sizeimage: w * h * 3 / 2,
        colorspace: V4L2_COLORSPACE_SRGB,
        ..Default::default()
    };
    // SAFETY: fd is a valid open V4L2 device; `fmt` has the kernel's layout.
    let r = unsafe {
        libc::ioctl(
            f.as_raw_fd(),
            VIDIOC_S_FMT as _,
            &mut fmt as *mut V4l2Format,
        )
    };
    if r < 0 {
        bail!("VIDIOC_S_FMT failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

// --- device discovery --------------------------------------------------------

/// Find the v4l2loopback device whose card label is "Linker Camera".
pub fn find_device() -> Option<PathBuf> {
    let entries = std::fs::read_dir("/sys/class/video4linux").ok()?;
    for e in entries.flatten() {
        let name = std::fs::read_to_string(e.path().join("name")).unwrap_or_default();
        if name.trim() == CARD_LABEL {
            return Some(Path::new("/dev").join(e.file_name()));
        }
    }
    None
}

/// Count processes other than us that have `dev` open.
fn count_consumers(dev: &Path) -> usize {
    let me = std::process::id();
    let Ok(target) = std::fs::canonicalize(dev) else {
        return 0;
    };
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return 0;
    };
    let mut n = 0;
    for p in procs.flatten() {
        let Some(pid) = p.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(p.path().join("fd")) else {
            continue;
        };
        if fds
            .flatten()
            .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|l| l == target))
        {
            n += 1;
        }
    }
    n
}

// --- sink ----------------------------------------------------------------------

pub struct V4l2Sink {
    path: PathBuf,
    file: File,
    w: u32,
    h: u32,
    frame: Vec<u8>,
    placeholder: Vec<u8>,
    decoder: Decoder,
}

pub fn open_sink(p: CameraParams) -> Cap<Box<dyn CameraSink>> {
    let path = find_device().ok_or_else(|| {
        Unsupported::new(format!(
            "no v4l2loopback device labelled \"{CARD_LABEL}\" — install v4l2loopback-dkms and load it \
             (see runbook: sudo modprobe v4l2loopback video_nr=10 card_label=\"{CARD_LABEL}\" exclusive_caps=1)"
        ))
    })?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| {
            Unsupported::new(format!(
                "cannot open {} ({e}) — add your user to the 'video' group",
                path.display()
            ))
        })?;
    set_output_format(&file, p.width, p.height).map_err(Unsupported::from)?;
    let decoder =
        Decoder::new().map_err(|e| Unsupported::new(format!("H.264 decoder init failed: {e}")))?;
    let size = (p.width * p.height * 3 / 2) as usize;
    tracing::info!(device = %path.display(), "virtual camera ready ({}x{} I420)", p.width, p.height);
    Ok(Box::new(V4l2Sink {
        path,
        file,
        w: p.width,
        h: p.height,
        frame: vec![0; size],
        placeholder: placeholder(p.width, p.height),
        decoder,
    }))
}

/// Dark gray frame with a lighter centered card (the "waiting" picture).
fn placeholder(w: u32, h: u32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut buf = vec![0u8; w * h * 3 / 2];
    let (y, uv) = buf.split_at_mut(w * h);
    for row in 0..h {
        for col in 0..w {
            let inside = row > h / 3 && row < 2 * h / 3 && col > w / 3 && col < 2 * w / 3;
            let stripe = inside && (row / 8) % 2 == 0;
            y[row * w + col] = if stripe {
                110
            } else if inside {
                90
            } else {
                40
            };
        }
    }
    uv.fill(128);
    buf
}

/// Copy a decoded picture into a packed I420 buffer of size `w`×`h`,
/// nearest-neighbour scaling if the stream resolution differs.
fn pack_i420(src: &impl YUVSource, w: usize, h: usize, out: &mut [u8]) {
    let (sw, sh) = src.dimensions();
    let (ys, us, vs) = src.strides();
    let (y, u, v) = (src.y(), src.u(), src.v());
    let (out_y, out_uv) = out.split_at_mut(w * h);
    let (out_u, out_v) = out_uv.split_at_mut((w / 2) * (h / 2));
    if sw == w && sh == h {
        for r in 0..h {
            out_y[r * w..r * w + w].copy_from_slice(&y[r * ys..r * ys + w]);
        }
        for r in 0..h / 2 {
            out_u[r * (w / 2)..(r + 1) * (w / 2)].copy_from_slice(&u[r * us..r * us + w / 2]);
            out_v[r * (w / 2)..(r + 1) * (w / 2)].copy_from_slice(&v[r * vs..r * vs + w / 2]);
        }
        return;
    }
    for r in 0..h {
        let sr = r * sh / h;
        for c in 0..w {
            out_y[r * w + c] = y[sr * ys + c * sw / w];
        }
    }
    for r in 0..h / 2 {
        let sr = r * (sh / 2) / (h / 2);
        for c in 0..w / 2 {
            let sc = c * (sw / 2) / (w / 2);
            out_u[r * (w / 2) + c] = u[sr * us + sc];
            out_v[r * (w / 2) + c] = v[sr * vs + sc];
        }
    }
}

impl CameraSink for V4l2Sink {
    fn write_encoded(&mut self, annexb: &[u8]) -> anyhow::Result<bool> {
        let (w, h) = (self.w as usize, self.h as usize);
        let Some(pic) = self.decoder.decode(annexb).context("H.264 decode")? else {
            return Ok(false);
        };
        pack_i420(&pic, w, h, &mut self.frame);
        self.file
            .write_all(&self.frame)
            .context("writing frame to v4l2loopback")?;
        Ok(true)
    }

    fn write_placeholder(&mut self) -> anyhow::Result<()> {
        self.file.write_all(&self.placeholder)?;
        Ok(())
    }

    fn consumers(&self) -> usize {
        count_consumers(&self.path)
    }

    fn reset_decoder(&mut self) {
        if let Ok(d) = Decoder::new() {
            self.decoder = d;
        }
    }

    fn device_path(&self) -> String {
        self.path.display().to_string()
    }

    fn decoder_name(&self) -> String {
        "openh264 (software)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4l2_format_abi_size() {
        // struct v4l2_format is 208 bytes on 64-bit Linux.
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<V4l2Format>(), 208);
        assert_eq!(std::mem::size_of::<V4l2PixFormat>(), 48);
        assert_eq!(VIDIOC_S_FMT, 0xC0D0_5605);
    }

    #[test]
    fn placeholder_is_valid_i420() {
        let p = placeholder(64, 48);
        assert_eq!(p.len(), 64 * 48 * 3 / 2);
        assert!(p[64 * 48..].iter().all(|&b| b == 128));
    }

    #[test]
    fn encode_decode_pack_roundtrip() {
        // Encode a synthetic frame with OpenH264 and decode it back through
        // the same path the sink uses.
        use openh264::encoder::Encoder;
        use openh264::formats::YUVBuffer;
        let (w, h) = (320usize, 240usize);
        let rgb: Vec<u8> = (0..w * h * 3).map(|i| ((i / 3) % 255) as u8).collect();
        let yuv = YUVBuffer::from_rgb_source(openh264::formats::RgbSliceU8::new(&rgb, (w, h)));
        let mut enc = Encoder::new().unwrap();
        let bs = enc.encode(&yuv).unwrap().to_vec();
        let mut dec = Decoder::new().unwrap();
        let pic = dec.decode(&bs).unwrap().expect("picture");
        let mut out = vec![0u8; w * h * 3 / 2];
        pack_i420(&pic, w, h, &mut out);
        // Downscale path too.
        let mut small = vec![0u8; 160 * 120 * 3 / 2];
        pack_i420(&pic, 160, 120, &mut small);
        assert!(out.iter().any(|&b| b != 0));
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use openh264::encoder::Encoder;
    use openh264::formats::YUVBuffer;
    use std::time::Instant;

    /// `cargo test --release -p linker-linux bench_decode_720p -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_decode_720p() {
        let (w, h) = (1280usize, 720usize);
        let mut enc = Encoder::new().unwrap();
        let mut frames = Vec::new();
        for f in 0..120usize {
            // Moving pseudo-random texture: compresses like real camera
            // footage (tens of KiB per frame), unlike a flat gradient.
            let rgb: Vec<u8> = (0..w * h * 3)
                .map(|i| {
                    let p = i / 3;
                    let (x, y) = ((p % w + f * 4) as u32, (p / w) as u32);
                    let mut v = x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B);
                    v ^= v >> 13;
                    ((v & 0x3F) as u8) + ((x / 16 + y / 16) % 2) as u8 * 120
                })
                .collect();
            let yuv = YUVBuffer::from_rgb_source(openh264::formats::RgbSliceU8::new(&rgb, (w, h)));
            frames.push(enc.encode(&yuv).unwrap().to_vec());
        }
        let mut dec = Decoder::new().unwrap();
        let mut out = vec![0u8; w * h * 3 / 2];
        let t = Instant::now();
        let mut n = 0;
        for f in &frames {
            if let Some(pic) = dec.decode(f).unwrap() {
                pack_i420(&pic, w, h, &mut out);
                n += 1;
            }
        }
        let per = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let kb: usize = frames.iter().map(|f| f.len()).sum::<usize>() / frames.len() / 1024;
        println!("720p decode+pack: {per:.2} ms/frame over {n} frames (avg {kb} KiB/frame) → {:.0}% of one core at 30 fps", per * 30.0 / 10.0);
    }
}
