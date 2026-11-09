//! KVM on macOS: `CGEventTap` capture (server role) and `CGEventPost`
//! injection (client role). Both need Accessibility and Input Monitoring
//! permission for linkerd.
//!
//! Coordinates: the "screen" is the bounding box of all active displays;
//! positions are reported relative to its top-left corner.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};

use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::display::{CGDisplay, CGPoint, CGRect};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CGMouseButton, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use linker_platform::{
    Cap, Edge, InputCapture, InputInjector, LocalInput, MouseButton, Unsupported,
};
use linker_proto::keycode::{hid_from_mac, mac_from_hid};
use tokio::sync::mpsc;

/// Union of all active display bounds (origin may be negative).
fn desktop_bounds() -> CGRect {
    let ids = CGDisplay::active_displays().unwrap_or_default();
    let mut rects = ids.into_iter().map(|id| CGDisplay::new(id).bounds());
    let first = rects.next().unwrap_or_else(|| CGDisplay::main().bounds());
    let (mut x0, mut y0) = (first.origin.x, first.origin.y);
    let (mut x1, mut y1) = (x0 + first.size.width, y0 + first.size.height);
    for r in rects {
        x0 = x0.min(r.origin.x);
        y0 = y0.min(r.origin.y);
        x1 = x1.max(r.origin.x + r.size.width);
        y1 = y1.max(r.origin.y + r.size.height);
    }
    CGRect::new(
        &CGPoint::new(x0, y0),
        &core_graphics::geometry::CGSize::new(x1 - x0, y1 - y0),
    )
}

fn edge_point(b: &CGRect, edge: Edge, ratio: f64) -> CGPoint {
    let r = ratio.clamp(0.0, 1.0);
    let (x0, y0, w, h) = (b.origin.x, b.origin.y, b.size.width, b.size.height);
    match edge {
        Edge::Left => CGPoint::new(x0 + 2.0, y0 + r * h),
        Edge::Right => CGPoint::new(x0 + w - 3.0, y0 + r * h),
        Edge::Top => CGPoint::new(x0 + r * w, y0 + 2.0),
        Edge::Bottom => CGPoint::new(x0 + r * w, y0 + h - 3.0),
    }
}

fn source() -> anyhow::Result<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource failed"))
}

fn modifier_flag(vk: u16) -> Option<CGEventFlags> {
    Some(match vk {
        0x38 | 0x3C => CGEventFlags::CGEventFlagShift,
        0x3B | 0x3E => CGEventFlags::CGEventFlagControl,
        0x3A | 0x3D => CGEventFlags::CGEventFlagAlternate,
        0x37 | 0x36 => CGEventFlags::CGEventFlagCommand,
        0x39 => CGEventFlags::CGEventFlagAlphaShift,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Capture (CGEventTap)
// ---------------------------------------------------------------------------

struct MacCapture {
    captured: Arc<AtomicBool>,
    bounds: CGRect,
}

pub fn capture(tx: mpsc::Sender<LocalInput>) -> Cap<Box<dyn InputCapture>> {
    let captured = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let c2 = captured.clone();
    std::thread::Builder::new()
        .name("linker-event-tap".into())
        .spawn(move || run_tap(tx, c2, ready_tx))
        .map_err(|e| Unsupported::new(e.to_string()))?;
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(Unsupported::new(e)),
        Err(_) => return Err(Unsupported::new("event tap thread died")),
    }
    Ok(Box::new(MacCapture {
        captured,
        bounds: desktop_bounds(),
    }))
}

fn run_tap(
    tx: mpsc::Sender<LocalInput>,
    captured: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let port: Arc<AtomicPtr<c_void>> = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
    let port2 = port.clone();
    let bounds = desktop_bounds();
    let types = vec![
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::ScrollWheel,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
    ];
    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        types,
        move |_proxy, etype, ev| {
            if matches!(
                etype,
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
            ) {
                let p = port2.load(Ordering::Relaxed);
                if !p.is_null() {
                    // SAFETY: p is our live tap's mach port.
                    unsafe { crate::ffi::CGEventTapEnable(p, true) };
                }
                return None;
            }
            if let Some(input) = translate(etype, ev, &bounds) {
                let _ = tx.try_send(input);
            }
            if captured.load(Ordering::Relaxed) {
                // Swallow: a Null-typed event is ignored by the system.
                ev.set_type(CGEventType::Null);
            }
            None
        },
    );
    let tap = match tap {
        Ok(t) => t,
        Err(()) => {
            let _ = ready.send(Err(
                "cannot create event tap — grant linkerd Accessibility and Input Monitoring \
                 (System Settings → Privacy & Security)"
                    .into(),
            ));
            return;
        }
    };
    port.store(
        tap.mach_port.as_concrete_TypeRef() as *mut c_void,
        Ordering::Relaxed,
    );
    let Ok(src) = tap.mach_port.create_runloop_source(0) else {
        let _ = ready.send(Err("event tap run loop source failed".into()));
        return;
    };
    let rl = CFRunLoop::get_current();
    // SAFETY: kCFRunLoopCommonModes is a valid CF constant.
    unsafe { rl.add_source(&src, kCFRunLoopCommonModes) };
    tap.enable();
    let _ = ready.send(Ok(()));
    CFRunLoop::run_current();
}

fn translate(etype: CGEventType, ev: &CGEvent, bounds: &CGRect) -> Option<LocalInput> {
    use CGEventType as T;
    Some(match etype {
        T::MouseMoved | T::LeftMouseDragged | T::RightMouseDragged | T::OtherMouseDragged => {
            let dx = ev.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as i32;
            let dy = ev.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as i32;
            let p = ev.location();
            LocalInput::Motion {
                dx,
                dy,
                pos: Some((p.x - bounds.origin.x, p.y - bounds.origin.y)),
            }
        }
        T::LeftMouseDown | T::LeftMouseUp => LocalInput::Button {
            button: MouseButton::Left,
            pressed: matches!(etype, T::LeftMouseDown),
        },
        T::RightMouseDown | T::RightMouseUp => LocalInput::Button {
            button: MouseButton::Right,
            pressed: matches!(etype, T::RightMouseDown),
        },
        T::OtherMouseDown | T::OtherMouseUp => {
            let button = match ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) {
                3 => MouseButton::Back,
                4 => MouseButton::Forward,
                _ => MouseButton::Middle,
            };
            LocalInput::Button {
                button,
                pressed: matches!(etype, T::OtherMouseDown),
            }
        }
        T::ScrollWheel => {
            let dy = ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1) as i32;
            let dx =
                -(ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2) as i32);
            if dx == 0 && dy == 0 {
                return None;
            }
            LocalInput::Scroll { dx, dy }
        }
        T::KeyDown | T::KeyUp => {
            if ev.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0 {
                return None;
            }
            let vk = ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            LocalInput::Key {
                hid: hid_from_mac(vk)?,
                pressed: matches!(etype, T::KeyDown),
            }
        }
        T::FlagsChanged => {
            let vk = ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let flag = modifier_flag(vk)?;
            LocalInput::Key {
                hid: hid_from_mac(vk)?,
                pressed: ev.get_flags().contains(flag),
            }
        }
        _ => return None,
    })
}

impl InputCapture for MacCapture {
    fn set_captured(&mut self, captured: bool) -> anyhow::Result<()> {
        self.captured.store(captured, Ordering::Relaxed);
        // Freeze/hide the local cursor while the peer has control (deltas
        // still arrive through the tap).
        let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(!captured);
        let main = CGDisplay::main();
        let _ = if captured {
            main.hide_cursor()
        } else {
            main.show_cursor()
        };
        Ok(())
    }

    fn screen_size(&self) -> (f64, f64) {
        (self.bounds.size.width, self.bounds.size.height)
    }

    fn warp_to(&mut self, edge: Edge, ratio: f64) -> anyhow::Result<()> {
        CGDisplay::warp_mouse_cursor_position(edge_point(&self.bounds, edge, ratio))
            .map_err(|e| anyhow::anyhow!("warp failed: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Injection (CGEventPost)
// ---------------------------------------------------------------------------

struct MacInjector {
    bounds: CGRect,
    buttons: HashSet<MouseButton>,
    flags: Mutex<CGEventFlags>,
}

pub fn injector() -> Cap<Box<dyn InputInjector>> {
    source().map_err(Unsupported::from)?;
    // SAFETY: AXIsProcessTrusted has no preconditions.
    if !unsafe { AXIsProcessTrusted() } {
        return Err(Unsupported::new(
            "linkerd is not trusted for Accessibility — enable it in System Settings → Privacy & Security → Accessibility",
        ));
    }
    Ok(Box::new(MacInjector {
        bounds: desktop_bounds(),
        buttons: HashSet::new(),
        flags: Mutex::new(CGEventFlags::empty()),
    }))
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

impl MacInjector {
    fn current(&self) -> CGPoint {
        source()
            .ok()
            .and_then(|s| CGEvent::new(s).ok())
            .map(|e| e.location())
            .unwrap_or(CGPoint::new(0.0, 0.0))
    }

    fn post_mouse(
        &self,
        ty: CGEventType,
        p: CGPoint,
        button: CGMouseButton,
        number: i64,
    ) -> anyhow::Result<()> {
        let ev = CGEvent::new_mouse_event(source()?, ty, p, button)
            .map_err(|_| anyhow::anyhow!("mouse event"))?;
        if number > 2 {
            ev.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
        }
        ev.set_flags(*self.flags.lock().expect("flags lock"));
        ev.post(CGEventTapLocation::HID);
        Ok(())
    }
}

impl InputInjector for MacInjector {
    fn move_relative(&mut self, dx: i32, dy: i32) -> anyhow::Result<()> {
        let b = self.bounds;
        let cur = self.current();
        let p = CGPoint::new(
            (cur.x + dx as f64).clamp(b.origin.x, b.origin.x + b.size.width - 1.0),
            (cur.y + dy as f64).clamp(b.origin.y, b.origin.y + b.size.height - 1.0),
        );
        let (ty, btn) = if self.buttons.contains(&MouseButton::Left) {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.buttons.contains(&MouseButton::Right) {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if !self.buttons.is_empty() {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        let ev = CGEvent::new_mouse_event(source()?, ty, p, btn)
            .map_err(|_| anyhow::anyhow!("mouse event"))?;
        ev.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx as i64);
        ev.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy as i64);
        ev.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn button(&mut self, button: MouseButton, pressed: bool) -> anyhow::Result<()> {
        if pressed {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        let p = self.current();
        let (down, up, cg, num) = match button {
            MouseButton::Left => (
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
                CGMouseButton::Left,
                0,
            ),
            MouseButton::Right => (
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
                CGMouseButton::Right,
                1,
            ),
            MouseButton::Middle => (
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                CGMouseButton::Center,
                2,
            ),
            MouseButton::Back => (
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                CGMouseButton::Center,
                3,
            ),
            MouseButton::Forward => (
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                CGMouseButton::Center,
                4,
            ),
        };
        self.post_mouse(if pressed { down } else { up }, p, cg, num)
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> anyhow::Result<()> {
        let ev = CGEvent::new_scroll_event(source()?, ScrollEventUnit::LINE, 2, dy, -dx, 0)
            .map_err(|_| anyhow::anyhow!("scroll event"))?;
        ev.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn key(&mut self, hid: u16, pressed: bool) -> anyhow::Result<()> {
        let vk = mac_from_hid(hid).ok_or_else(|| anyhow::anyhow!("no mac key for HID {hid:#x}"))?;
        let flags = {
            let mut f = self.flags.lock().expect("flags lock");
            if let Some(flag) = modifier_flag(vk) {
                if flag != CGEventFlags::CGEventFlagAlphaShift {
                    f.set(flag, pressed);
                }
            }
            *f
        };
        let ev = CGEvent::new_keyboard_event(source()?, vk, pressed)
            .map_err(|_| anyhow::anyhow!("key event"))?;
        if modifier_flag(vk).is_some() {
            ev.set_type(CGEventType::FlagsChanged);
        }
        ev.set_flags(flags);
        ev.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn warp_to(&mut self, edge: Edge, ratio: f64) -> anyhow::Result<()> {
        let p = edge_point(&self.bounds, edge, ratio);
        CGDisplay::warp_mouse_cursor_position(p)
            .map_err(|e| anyhow::anyhow!("warp failed: {e}"))?;
        // Warping doesn't generate an event; post one so apps see the move.
        self.post_mouse(CGEventType::MouseMoved, p, CGMouseButton::Left, 0)
    }

    fn cursor_position(&self) -> Option<(f64, f64)> {
        let p = self.current();
        Some((p.x - self.bounds.origin.x, p.y - self.bounds.origin.y))
    }

    fn screen_size(&self) -> (f64, f64) {
        (self.bounds.size.width, self.bounds.size.height)
    }
}

// The injector/capture hold only plain data and CF objects used from one
// thread at a time (the KVM task).
unsafe impl Send for MacInjector {}
unsafe impl Send for MacCapture {}
