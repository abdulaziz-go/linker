//! Camera source: AVFoundation capture → VideoToolbox hardware H.264.
//! **Video only. Linker never opens the microphone.**
//!
//! Encoder settings (low latency): real-time mode, Baseline profile (no
//! B-frames — also guarantees the Ubuntu decoder can handle it), frame
//! reordering off, average bitrate + 1 s data-rate cap, keyframe every 2 s
//! plus on demand. Frames are encoded synchronously in the capture callback
//! and converted from AVCC to Annex-B with SPS/PPS before every IDR.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use linker_platform::{
    now_us, CamState, CameraEvent, CameraParams, CameraSource, Cap, EncodedFrame, Unsupported,
};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObjectProtocol};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_foundation::{NSObject, NSString};
use tokio::sync::mpsc;

use crate::ffi::*;
use crate::objc::{class, ns, nsdict, nsnumber_u32};

struct Shared {
    tx: mpsc::Sender<CameraEvent>,
    session: AtomicPtr<c_void>,
    force_key: AtomicBool,
    bitrate_kbps: AtomicU32,
    applied_kbps: AtomicU32,
    last_frame_us: AtomicU64,
    last_encoded_us: AtomicU64,
    min_interval_us: u64,
    stopped: AtomicBool,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "LinkerCameraFrameDelegate"]
    #[ivars = Arc<Shared>]
    struct FrameDelegate;

    impl FrameDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, _output: *mut AnyObject, sample: *mut c_void, _conn: *mut AnyObject) {
            on_frame(self.ivars(), sample);
        }

        #[unsafe(method(captureOutput:didDropSampleBuffer:fromConnection:))]
        fn did_drop(&self, _output: *mut AnyObject, _sample: *mut c_void, _conn: *mut AnyObject) {}
    }

    unsafe impl NSObjectProtocol for FrameDelegate {}
);

impl FrameDelegate {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}

fn cfstr(s: CFStringRef) -> CFString {
    // SAFETY: `s` is a framework-exported constant CFString.
    unsafe { CFString::wrap_under_get_rule(s as core_foundation::string::CFStringRef) }
}

fn set_prop(session: *mut c_void, key: CFStringRef, value: &CFType) -> OSStatus {
    // SAFETY: valid session and CF objects.
    unsafe { VTSessionSetProperty(session, key, value.as_CFTypeRef()) }
}

fn apply_bitrate(session: *mut c_void, kbps: u32) {
    let bps = kbps as i64 * 1000;
    let avg = CFNumber::from(bps);
    // Cap bursts: at most 1.5× the average in any one-second window.
    let limits = CFArray::from_CFTypes(&[CFNumber::from(bps * 3 / 2 / 8), CFNumber::from(1i32)]);
    // SAFETY: framework constants.
    unsafe {
        set_prop(
            session,
            kVTCompressionPropertyKey_AverageBitRate,
            &avg.as_CFType(),
        );
        set_prop(
            session,
            kVTCompressionPropertyKey_DataRateLimits,
            &limits.as_CFType(),
        );
    }
}

fn on_frame(sh: &Shared, sample: CMSampleBufferRef) {
    if sh.stopped.load(Ordering::Relaxed) || sample.is_null() {
        return;
    }
    let now = now_us();
    sh.last_frame_us.store(now, Ordering::Relaxed);
    // Respect the requested fps if the camera delivers more.
    let last = sh.last_encoded_us.load(Ordering::Relaxed);
    if now.saturating_sub(last) < sh.min_interval_us * 8 / 10 {
        return;
    }
    sh.last_encoded_us.store(now, Ordering::Relaxed);
    let session = sh.session.load(Ordering::Acquire);
    if session.is_null() {
        return;
    }
    let want = sh.bitrate_kbps.load(Ordering::Relaxed);
    if sh.applied_kbps.swap(want, Ordering::Relaxed) != want {
        apply_bitrate(session, want);
    }
    // SAFETY: `sample` is valid for the duration of this callback; VT
    // retains the image buffer it needs.
    unsafe {
        let image = CMSampleBufferGetImageBuffer(sample);
        if image.is_null() {
            return;
        }
        let pts = CMSampleBufferGetPresentationTimeStamp(sample);
        let props = sh.force_key.swap(false, Ordering::Relaxed).then(|| {
            CFDictionary::from_CFType_pairs(&[(
                cfstr(kVTEncodeFrameOptionKey_ForceKeyFrame).as_CFType(),
                CFBoolean::true_value().as_CFType(),
            )])
        });
        let props_ref = props.as_ref().map_or(std::ptr::null(), |d| {
            d.as_concrete_TypeRef() as CFDictionaryRef
        });
        let st = VTCompressionSessionEncodeFrame(
            session,
            image,
            pts,
            kCMTimeInvalid,
            props_ref,
            now as usize as *mut c_void,
            std::ptr::null_mut(),
        );
        if st != 0 {
            tracing::debug!(status = st, "VTCompressionSessionEncodeFrame failed");
        }
    }
}

extern "C" fn vt_output(
    refcon: *mut c_void,
    src: *mut c_void,
    status: OSStatus,
    _flags: u32,
    sb: CMSampleBufferRef,
) {
    if status != 0 || sb.is_null() || refcon.is_null() {
        return;
    }
    // SAFETY: refcon is the Arc<Shared> raw pointer we registered; it is
    // only freed after the session is invalidated (no more callbacks).
    let sh = unsafe { &*(refcon as *const Shared) };
    if sh.stopped.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: `sb` is a valid encoded sample buffer during the callback.
    unsafe {
        if CMSampleBufferDataIsReady(sb) == 0 {
            return;
        }
        // Keyframe ⇔ attachment NotSync absent.
        let atts = CMSampleBufferGetSampleAttachmentsArray(sb, 0);
        let keyframe = if atts.is_null() || CFArrayGetCount(atts) == 0 {
            true
        } else {
            let d = CFArrayGetValueAtIndex(atts, 0);
            CFDictionaryContainsKey(d, kCMSampleAttachmentKey_NotSync) == 0
        };
        let mut out = Vec::with_capacity(64 * 1024);
        let mut nal_len = 4i32;
        if keyframe {
            let fmt = CMSampleBufferGetFormatDescription(sb);
            let mut count = 0usize;
            let _ = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                fmt,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut count,
                &mut nal_len,
            );
            for i in 0..count {
                let mut ptr: *const u8 = std::ptr::null();
                let mut size = 0usize;
                if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    fmt,
                    i,
                    &mut ptr,
                    &mut size,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ) == 0
                    && !ptr.is_null()
                {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(std::slice::from_raw_parts(ptr, size));
                }
            }
        }
        let block = CMSampleBufferGetDataBuffer(sb);
        if block.is_null() {
            return;
        }
        let len = CMBlockBufferGetDataLength(block);
        let mut avcc = vec![0u8; len];
        if CMBlockBufferCopyDataBytes(block, 0, len, avcc.as_mut_ptr() as *mut c_void) != 0 {
            return;
        }
        avcc_to_annexb(&avcc, nal_len.clamp(1, 4) as usize, &mut out);
        let _ = sh.tx.try_send(CameraEvent::Frame(EncodedFrame {
            data: out,
            keyframe,
            capture_ts_us: src as usize as u64,
        }));
    }
}

/// Objective-C objects of a running capture. Used from one thread at a time.
struct Parts {
    session: Retained<AnyObject>,
    _output: Retained<AnyObject>,
    _delegate: Retained<FrameDelegate>,
    _queue: Retained<AnyObject>,
}
// SAFETY: AVCaptureSession start/stop are thread-safe; the rest is inert.
unsafe impl Send for Parts {}

pub struct MacCamera {
    shared: Arc<Shared>,
    parts: Option<Parts>,
    refcon: *const Shared,
}
// SAFETY: raw pointer is only used for teardown.
unsafe impl Send for MacCamera {}

fn authorize(media: &NSString) -> Result<(), Unsupported> {
    let cls = class("AVCaptureDevice").map_err(Unsupported::from)?;
    // SAFETY: standard AVFoundation API.
    let status: isize = unsafe { msg_send![cls, authorizationStatusForMediaType: media] };
    match status {
        3 => Ok(()),
        0 => {
            let (tx, rx) = std::sync::mpsc::channel::<bool>();
            let block = block2::RcBlock::new(move |granted: Bool| {
                let _ = tx.send(granted.as_bool());
            });
            // SAFETY: standard AVFoundation API; the block is copied by the callee.
            let _: () = unsafe { msg_send![cls, requestAccessForMediaType: media, completionHandler: &*block] };
            match rx.recv_timeout(Duration::from_secs(60)) {
                Ok(true) => Ok(()),
                _ => Err(Unsupported::new("camera access was not granted")),
            }
        }
        _ => Err(Unsupported::new(
            "camera permission denied — enable linkerd in System Settings → Privacy & Security → Camera",
        )),
    }
}

fn preset_for(height: u32) -> &'static str {
    match height {
        h if h >= 1080 => "AVCaptureSessionPreset1920x1080",
        h if h >= 720 => "AVCaptureSessionPreset1280x720",
        h if h >= 480 => "AVCaptureSessionPreset640x480",
        _ => "AVCaptureSessionPresetMedium",
    }
}

fn create_encoder(p: &CameraParams, refcon: *const Shared) -> anyhow::Result<*mut c_void> {
    // SAFETY: standard VideoToolbox calls with valid arguments.
    unsafe {
        let spec = CFDictionary::from_CFType_pairs(&[(
            cfstr(kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder).as_CFType(),
            CFBoolean::true_value().as_CFType(),
        )]);
        let mut session: VTCompressionSessionRef = std::ptr::null_mut();
        let st = VTCompressionSessionCreate(
            std::ptr::null(),
            p.width as i32,
            p.height as i32,
            kCMVideoCodecType_H264,
            spec.as_concrete_TypeRef() as CFDictionaryRef,
            std::ptr::null(),
            std::ptr::null(),
            vt_output,
            refcon as *mut c_void,
            &mut session,
        );
        if st != 0 || session.is_null() {
            anyhow::bail!("VTCompressionSessionCreate failed ({st})");
        }
        let t = CFBoolean::true_value().as_CFType();
        let f = CFBoolean::false_value().as_CFType();
        set_prop(session, kVTCompressionPropertyKey_RealTime, &t);
        set_prop(session, kVTCompressionPropertyKey_AllowFrameReordering, &f);
        set_prop(
            session,
            kVTCompressionPropertyKey_ProfileLevel,
            &cfstr(kVTProfileLevel_H264_Baseline_AutoLevel).as_CFType(),
        );
        set_prop(
            session,
            kVTCompressionPropertyKey_ExpectedFrameRate,
            &CFNumber::from(p.fps as i32).as_CFType(),
        );
        set_prop(
            session,
            kVTCompressionPropertyKey_MaxKeyFrameInterval,
            &CFNumber::from((p.fps * 2) as i32).as_CFType(),
        );
        set_prop(
            session,
            kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
            &CFNumber::from(2i32).as_CFType(),
        );
        apply_bitrate(session, p.bitrate_kbps);
        VTCompressionSessionPrepareToEncodeFrames(session);
        Ok(session)
    }
}

pub fn start(p: CameraParams, tx: mpsc::Sender<CameraEvent>) -> Cap<Box<dyn CameraSource>> {
    let shared = Arc::new(Shared {
        tx: tx.clone(),
        session: AtomicPtr::new(std::ptr::null_mut()),
        force_key: AtomicBool::new(true),
        bitrate_kbps: AtomicU32::new(p.bitrate_kbps),
        applied_kbps: AtomicU32::new(p.bitrate_kbps),
        last_frame_us: AtomicU64::new(0),
        last_encoded_us: AtomicU64::new(0),
        min_interval_us: 1_000_000 / p.fps.max(1) as u64,
        stopped: AtomicBool::new(false),
    });
    let refcon = Arc::into_raw(shared.clone());
    let mut cam = MacCamera {
        shared: shared.clone(),
        parts: None,
        refcon,
    };

    let session = create_encoder(&p, refcon).map_err(Unsupported::from)?;
    shared.session.store(session, Ordering::Release);

    let parts = objc2::rc::autoreleasepool(|_| -> anyhow::Result<Parts> {
        // SAFETY: standard AVFoundation calls; all objects are retained.
        unsafe {
            let media: &NSString = crate::objc::AVMediaTypeVideo;
            authorize(media).map_err(|e| anyhow::anyhow!(e.0))?;
            let dev_cls = class("AVCaptureDevice")?;
            let device: Option<Retained<AnyObject>> =
                msg_send![dev_cls, defaultDeviceWithMediaType: media];
            let device = device.ok_or_else(|| anyhow::anyhow!("no camera found"))?;
            let input_cls = class("AVCaptureDeviceInput")?;
            let input: Option<Retained<AnyObject>> = msg_send![
                input_cls,
                deviceInputWithDevice: &*device,
                error: std::ptr::null_mut::<*mut AnyObject>()
            ];
            let input = input
                .ok_or_else(|| anyhow::anyhow!("camera is unavailable (in use or restricted)"))?;

            let session: Retained<AnyObject> = msg_send![class("AVCaptureSession")?, new];
            let _: () = msg_send![&*session, beginConfiguration];
            let preset = ns(preset_for(p.height));
            let can: Bool = msg_send![&*session, canSetSessionPreset: &*preset];
            if can.as_bool() {
                let _: () = msg_send![&*session, setSessionPreset: &*preset];
            }
            let can: Bool = msg_send![&*session, canAddInput: &*input];
            if !can.as_bool() {
                anyhow::bail!("cannot add camera input");
            }
            let _: () = msg_send![&*session, addInput: &*input];

            let output: Retained<AnyObject> = msg_send![class("AVCaptureVideoDataOutput")?, new];
            let k_fmt = ns("PixelFormatType");
            let k_w = ns("Width");
            let k_h = ns("Height");
            let v_fmt = nsnumber_u32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)?;
            let v_w = nsnumber_u32(p.width)?;
            let v_h = nsnumber_u32(p.height)?;
            let settings = nsdict(&[(&k_fmt, &v_fmt), (&k_w, &v_w), (&k_h, &v_h)])?;
            let _: () = msg_send![&*output, setVideoSettings: &*settings];
            let _: () = msg_send![&*output, setAlwaysDiscardsLateVideoFrames: Bool::YES];

            let label = std::ffi::CString::new("io.linker.camera")?;
            let q = dispatch_queue_create(label.as_ptr(), std::ptr::null());
            let queue: Retained<AnyObject> = Retained::from_raw(q as *mut AnyObject)
                .ok_or_else(|| anyhow::anyhow!("dispatch queue"))?;
            let delegate = FrameDelegate::new(shared.clone());
            let _: () = msg_send![&*output, setSampleBufferDelegate: &*delegate, queue: &*queue];
            let can: Bool = msg_send![&*session, canAddOutput: &*output];
            if !can.as_bool() {
                anyhow::bail!("cannot add video output");
            }
            let _: () = msg_send![&*session, addOutput: &*output];
            let _: () = msg_send![&*session, commitConfiguration];
            let _: () = msg_send![&*session, startRunning];
            Ok(Parts {
                session,
                _output: output,
                _delegate: delegate,
                _queue: queue,
            })
        }
    });
    match parts {
        Ok(parts) => cam.parts = Some(parts),
        Err(e) => {
            // `cam`'s Drop releases the encoder.
            return Err(Unsupported::new(format!("{e:#}")));
        }
    }

    // Watchdog: report Busy if frames stop (camera taken / lid closed).
    let sh = shared.clone();
    std::thread::Builder::new()
        .name("linker-camera-watchdog".into())
        .spawn(move || {
            let started = now_us();
            let mut streaming = false;
            while !sh.stopped.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                let last = sh.last_frame_us.load(Ordering::Relaxed);
                let alive = last > 0 && now_us().saturating_sub(last) < 2_000_000;
                if alive != streaming && (alive || now_us().saturating_sub(started) > 3_000_000) {
                    streaming = alive;
                    let (state, detail) = if alive {
                        (CamState::Streaming, "camera streaming".to_string())
                    } else {
                        (
                            CamState::Busy,
                            "no frames from camera (in use by another app or lid closed?)"
                                .to_string(),
                        )
                    };
                    let _ = sh.tx.try_send(CameraEvent::State { state, detail });
                }
            }
        })
        .map_err(|e| Unsupported::new(e.to_string()))?;

    tracing::info!(
        "camera capture started ({}x{}@{}, {} kbps, VideoToolbox H.264)",
        p.width,
        p.height,
        p.fps,
        p.bitrate_kbps
    );
    Ok(Box::new(cam))
}

impl CameraSource for MacCamera {
    fn request_keyframe(&self) {
        self.shared.force_key.store(true, Ordering::Relaxed);
    }
    fn set_bitrate(&self, kbps: u32) {
        self.shared.bitrate_kbps.store(kbps, Ordering::Relaxed);
    }
}

impl Drop for MacCamera {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        let parts = self.parts.take();
        let session = self
            .shared
            .session
            .swap(std::ptr::null_mut(), Ordering::AcqRel) as usize;
        let refcon = self.refcon as usize;
        // stopRunning blocks; tear down off the async runtime.
        std::thread::spawn(move || {
            if let Some(p) = parts {
                // SAFETY: valid capture session.
                unsafe {
                    let _: () = msg_send![&*p.session, stopRunning];
                }
                drop(p);
            }
            let session = session as *mut c_void;
            if !session.is_null() {
                // SAFETY: we own the session; after Invalidate no callbacks run.
                unsafe {
                    VTCompressionSessionCompleteFrames(session, kCMTimeInvalid);
                    VTCompressionSessionInvalidate(session);
                    CFRelease(session as CFTypeRef);
                }
            }
            if refcon != 0 {
                // SAFETY: matches Arc::into_raw in `start`.
                unsafe { drop(Arc::from_raw(refcon as *const Shared)) };
            }
            tracing::info!("camera released");
        });
    }
}
