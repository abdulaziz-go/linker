//! Small helpers for dynamic Objective-C messaging.

use std::ffi::c_void;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::NSString;

// Reference one real symbol from each Objective-C framework so the linker
// keeps them (their classes are looked up at runtime by name).
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {
    pub static AVMediaTypeVideo: &'static NSString;
}
#[link(name = "AppKit", kind = "framework")]
extern "C" {
    pub static NSPasteboardTypeString: &'static NSString;
}

pub fn class(name: &str) -> anyhow::Result<&'static AnyClass> {
    // Touch the framework symbols so they're linked.
    // SAFETY: reading framework-exported constant pointers.
    let _ = unsafe {
        (
            AVMediaTypeVideo as *const NSString,
            NSPasteboardTypeString as *const NSString,
        )
    };
    let c = std::ffi::CString::new(name)?;
    AnyClass::get(&c).ok_or_else(|| anyhow::anyhow!("Objective-C class {name} not available"))
}

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

pub fn nsdata(bytes: &[u8]) -> anyhow::Result<Retained<AnyObject>> {
    let cls = class("NSData")?;
    // SAFETY: dataWithBytes:length: copies the buffer.
    let d: Option<Retained<AnyObject>> = unsafe {
        msg_send![cls, dataWithBytes: bytes.as_ptr() as *const c_void, length: bytes.len()]
    };
    d.ok_or_else(|| anyhow::anyhow!("NSData alloc failed"))
}

pub fn nsdata_to_vec(d: &AnyObject) -> Vec<u8> {
    // SAFETY: NSData's bytes pointer is valid for `length` bytes while `d` lives.
    unsafe {
        let len: usize = msg_send![d, length];
        let ptr: *const c_void = msg_send![d, bytes];
        if ptr.is_null() || len == 0 {
            return Vec::new();
        }
        std::slice::from_raw_parts(ptr as *const u8, len).to_vec()
    }
}

/// Mutable dictionary built from (key, value) object pairs.
pub fn nsdict(pairs: &[(&AnyObject, &AnyObject)]) -> anyhow::Result<Retained<AnyObject>> {
    let cls = class("NSMutableDictionary")?;
    // SAFETY: standard Foundation API.
    unsafe {
        let d: Retained<AnyObject> = msg_send![cls, new];
        for (k, v) in pairs {
            let _: () = msg_send![&*d, setObject: *v, forKey: *k];
        }
        Ok(d)
    }
}

pub fn nsnumber_u32(v: u32) -> anyhow::Result<Retained<AnyObject>> {
    let cls = class("NSNumber")?;
    // SAFETY: standard Foundation API.
    Ok(unsafe { msg_send![cls, numberWithUnsignedInt: v] })
}

/// `[NSArray count]` + `objectAtIndex:` → Vec of retained objects.
pub fn nsarray_items(arr: &AnyObject) -> Vec<Retained<AnyObject>> {
    // SAFETY: standard Foundation API.
    unsafe {
        let n: usize = msg_send![arr, count];
        (0..n).map(|i| msg_send![arr, objectAtIndex: i]).collect()
    }
}

pub fn as_nsstring(o: &AnyObject) -> String {
    // SAFETY: caller passes an NSString.
    let s: &NSString = unsafe { &*(o as *const AnyObject as *const NSString) };
    s.to_string()
}

/// macOS product version, e.g. "15.1".
pub fn macos_version() -> String {
    std::process::Command::new("sysctl")
        .args(["-n", "kern.osproductversion"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn macos_major() -> u32 {
    macos_version()
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0)
}
