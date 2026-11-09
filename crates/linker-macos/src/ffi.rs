//! Raw C bindings for the few CoreFoundation / CoreMedia / CoreVideo /
//! VideoToolbox entry points we use. These are stable, documented C ABIs.

#![allow(non_upper_case_globals, non_snake_case, dead_code)]

use std::ffi::c_void;

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type OSStatus = i32;
pub type Boolean = u8;

pub type CMSampleBufferRef = *mut c_void;
pub type CMBlockBufferRef = *mut c_void;
pub type CMFormatDescriptionRef = *mut c_void;
pub type CVImageBufferRef = *mut c_void;
pub type VTCompressionSessionRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

pub const kCMVideoCodecType_H264: u32 = u32::from_be_bytes(*b"avc1");
/// '420v' — NV12 video range, the camera's native format (no conversion).
pub const kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange: u32 = u32::from_be_bytes(*b"420v");

pub type VTCompressionOutputCallback = extern "C" fn(
    output_refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    sample_buffer: CMSampleBufferRef,
);

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFArrayGetCount(a: CFArrayRef) -> isize;
    pub fn CFArrayGetValueAtIndex(a: CFArrayRef, idx: isize) -> *const c_void;
    pub fn CFDictionaryContainsKey(d: CFDictionaryRef, key: *const c_void) -> Boolean;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    pub static kCMTimeInvalid: CMTime;
    pub static kCMSampleAttachmentKey_NotSync: CFStringRef;

    pub fn CMSampleBufferGetImageBuffer(sb: CMSampleBufferRef) -> CVImageBufferRef;
    pub fn CMSampleBufferGetPresentationTimeStamp(sb: CMSampleBufferRef) -> CMTime;
    pub fn CMSampleBufferGetDataBuffer(sb: CMSampleBufferRef) -> CMBlockBufferRef;
    pub fn CMSampleBufferGetFormatDescription(sb: CMSampleBufferRef) -> CMFormatDescriptionRef;
    pub fn CMSampleBufferGetSampleAttachmentsArray(
        sb: CMSampleBufferRef,
        create: Boolean,
    ) -> CFArrayRef;
    pub fn CMSampleBufferDataIsReady(sb: CMSampleBufferRef) -> Boolean;
    pub fn CMBlockBufferGetDataLength(b: CMBlockBufferRef) -> usize;
    pub fn CMBlockBufferCopyDataBytes(
        b: CMBlockBufferRef,
        offset: usize,
        len: usize,
        dst: *mut c_void,
    ) -> OSStatus;
    pub fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        desc: CMFormatDescriptionRef,
        index: usize,
        ptr_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_len_out: *mut i32,
    ) -> OSStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    pub fn CVPixelBufferGetWidth(pb: CVImageBufferRef) -> usize;
    pub fn CVPixelBufferGetHeight(pb: CVImageBufferRef) -> usize;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    pub static kVTCompressionPropertyKey_RealTime: CFStringRef;
    pub static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    pub static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    pub static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    pub static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
    pub static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    pub static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: CFStringRef;
    pub static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    pub static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
    pub static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    pub static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFStringRef;

    pub fn VTCompressionSessionCreate(
        allocator: CFAllocatorRef,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: CFDictionaryRef,
        source_image_buffer_attributes: CFDictionaryRef,
        compressed_data_allocator: CFAllocatorRef,
        output_callback: VTCompressionOutputCallback,
        output_callback_refcon: *mut c_void,
        session_out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    pub fn VTSessionSetProperty(
        session: *mut c_void,
        key: CFStringRef,
        value: CFTypeRef,
    ) -> OSStatus;
    pub fn VTCompressionSessionPrepareToEncodeFrames(session: VTCompressionSessionRef) -> OSStatus;
    pub fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image_buffer: CVImageBufferRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_frame_refcon: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    pub fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        complete_until: CMTime,
    ) -> OSStatus;
    pub fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGEventTapEnable(tap: *mut c_void, enable: bool);
}

extern "C" {
    pub fn dispatch_queue_create(
        label: *const std::ffi::c_char,
        attr: *const c_void,
    ) -> *mut c_void;
}

/// Convert AVCC (length-prefixed NAL units) to Annex-B (start codes).
pub fn avcc_to_annexb(avcc: &[u8], nal_len_size: usize, out: &mut Vec<u8>) {
    let mut i = 0;
    while i + nal_len_size <= avcc.len() {
        let mut len = 0usize;
        for b in &avcc[i..i + nal_len_size] {
            len = (len << 8) | *b as usize;
        }
        i += nal_len_size;
        if i + len > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[i..i + len]);
        i += len;
    }
}
