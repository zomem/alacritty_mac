use std::sync::atomic::{AtomicPtr, AtomicBool, Ordering, AtomicU32};
use std::sync::OnceLock;
use std::ptr;

use winit::event_loop::EventLoopProxy;

use crate::event::{Event, EventType};
use objc2::class;
use objc2::runtime::AnyObject;
use objc2::msg_send;

// 通过 Carbon 注册系统级全局热键（无需依赖 block）。
// 仅支持功能键（F1..F19）与“无修饰”的简单场景，满足“显示/隐藏全部窗口”的需求。

#[allow(non_camel_case_types)]
type OSStatus = i32;
#[allow(non_camel_case_types)]
type EventRef = *mut std::ffi::c_void;
#[allow(non_camel_case_types)]
type EventHandlerCallRef = *mut std::ffi::c_void;
#[allow(non_camel_case_types)]
type EventTargetRef = *mut std::ffi::c_void;
#[allow(non_camel_case_types)]
type EventHandlerRef = *mut std::ffi::c_void;
#[allow(non_camel_case_types)]
type EventHotKeyRef = *mut std::ffi::c_void;

#[repr(C)]
struct EventTypeSpec {
    eventClass: u32,
    eventKind: u32,
}

#[repr(C)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn GetEventDispatcherTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: extern "C" fn(EventHandlerCallRef, EventRef, *mut std::ffi::c_void) -> OSStatus,
        num_types: u32,
        types: *const EventTypeSpec,
        user_data: *mut std::ffi::c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        hot_key_id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(hk: EventHotKeyRef) -> OSStatus;
}

// kEventClassKeyboard = FOUR_CHAR_CODE('kbd ')
const K_EVENT_CLASS_KEYBOARD: u32 = ((b'k' as u32) << 24)
    | ((b'b' as u32) << 16)
    | ((b'd' as u32) << 8)
    | (b' ' as u32);
// kEventHotKeyPressed = 5（Carbon 常量）
const K_EVENT_HOTKEY_PRESSED: u32 = 5;

static HOTKEY_REF: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(ptr::null_mut());
static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

// 事件代理（拥有所有权，避免悬垂指针）。
static EVENT_PROXY: OnceLock<EventLoopProxy<Event>> = OnceLock::new();
static GLOBAL_MONITOR: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(ptr::null_mut());
static CURRENT_CODE: AtomicU32 = AtomicU32::new(u32::MAX);
static CURRENT_MODS: AtomicU32 = AtomicU32::new(0);

// CoreGraphics 事件 Tap 兜底（需要“输入监控”权限）
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(location: u32, placement: u32, options: u32, eventsOfInterest: u64,
        callback: extern "C" fn(*mut std::ffi::c_void, u32, *mut std::ffi::c_void, *mut std::ffi::c_void) -> *mut std::ffi::c_void,
        user_info: *mut std::ffi::c_void) -> *mut std::ffi::c_void; // CFMachPortRef
    fn CGEventTapEnable(tap: *mut std::ffi::c_void, enable: bool);
    fn CGEventGetIntegerValueField(ev: *mut std::ffi::c_void, field: u32) -> i64;
    fn CGEventGetFlags(ev: *mut std::ffi::c_void) -> u64;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFMachPortCreateRunLoopSource(allocator: *const std::ffi::c_void, tap: *mut std::ffi::c_void, order: i64) -> *mut std::ffi::c_void; // CFRunLoopSourceRef
    fn CFRunLoopGetMain() -> *mut std::ffi::c_void; // CFRunLoopRef
    fn CFRunLoopAddSource(rl: *mut std::ffi::c_void, source: *mut std::ffi::c_void, mode: *const std::ffi::c_void);
    static kCFRunLoopCommonModes: *const std::ffi::c_void; // CFStringRef
    fn CFRelease(obj: *const std::ffi::c_void);
}

const KCG_SESSION_EVENT_TAP: u32 = 1; // kCGSessionEventTap
const KCG_HEAD_INSERT_EVENT_TAP: u32 = 0; // kCGHeadInsertEventTap
const KCG_TAP_OPTION_LISTEN_ONLY: u32 = 1; // kCGEventTapOptionListenOnly
const KCG_EVENT_KEY_DOWN: u32 = 10; // kCGEventKeyDown
const KCG_KEYBOARD_EVENT_KEYCODE: u32 = 9; // kCGKeyboardEventKeycode

/// 注入 EventLoopProxy（拥有一份拷贝）。
pub fn set_event_proxy(proxy: EventLoopProxy<Event>) {
    let _ = EVENT_PROXY.set(proxy);
}

extern "C" fn hotkey_handler(
    _next: EventHandlerCallRef,
    _event: EventRef,
    _user_data: *mut std::ffi::c_void,
) -> OSStatus {
    eprintln!("[hotkey] pressed event received");
    if let Some(proxy) = EVENT_PROXY.get() {
        let _ = proxy.send_event(Event::new(EventType::ToggleAllWindows, None));
    }
    0
}

// monitor 回调未启用

fn ensure_handler_installed() {
    if HANDLER_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let spec_pressed = EventTypeSpec { eventClass: K_EVENT_CLASS_KEYBOARD, eventKind: K_EVENT_HOTKEY_PRESSED };
        let mut handler_ref1: EventHandlerRef = ptr::null_mut();
        let status1 = InstallEventHandler(
            GetApplicationEventTarget(),
            hotkey_handler,
            1,
            &spec_pressed as *const EventTypeSpec,
            ptr::null_mut(),
            &mut handler_ref1 as *mut _,
        );
        eprintln!("[hotkey] handler(app) install status: {}", status1);

        let mut handler_ref2: EventHandlerRef = ptr::null_mut();
        let status2 = InstallEventHandler(
            GetEventDispatcherTarget(),
            hotkey_handler,
            1,
            &spec_pressed as *const EventTypeSpec,
            ptr::null_mut(),
            &mut handler_ref2 as *mut _,
        );
        eprintln!("[hotkey] handler(dispatch) install status: {}", status2);
    }
}

fn unregister_current() {
    let hk = HOTKEY_REF.swap(ptr::null_mut(), Ordering::SeqCst);
    if !hk.is_null() {
        unsafe { let _ = UnregisterEventHotKey(hk); }
    }
}

fn uninstall_global_monitor() {
    // 停用并释放 CGEventTap 资源
    let tap = GLOBAL_MONITOR.swap(ptr::null_mut(), Ordering::SeqCst);
    if !tap.is_null() {
        unsafe {
            CGEventTapEnable(tap, false);
            CFRelease(tap);
        }
    }
}

extern "C" fn tap_cb(
    _proxy: *mut std::ffi::c_void,
    typ: u32,
    event: *mut std::ffi::c_void,
    _user: *mut std::ffi::c_void,
) -> *mut std::ffi::c_void {
    if typ == KCG_EVENT_KEY_DOWN && !event.is_null() {
        unsafe {
            let code = CGEventGetIntegerValueField(event, KCG_KEYBOARD_EVENT_KEYCODE) as u32;
            let flags = CGEventGetFlags(event);
            let want_code = CURRENT_CODE.load(Ordering::Relaxed);
            let want_mods = CURRENT_MODS.load(Ordering::Relaxed);
            if code == want_code {
                let cur = nsflags_to_carbon_modifiers(flags);
                if cur == want_mods {
                    if let Some(proxy) = EVENT_PROXY.get() {
                        let _ = proxy.send_event(Event::new(EventType::ToggleAllWindows, None));
                    }
                }
            }
        }
    }
    event
}

fn install_global_monitor_for_combo(code: u16, carbon_mods: u32) {
    CURRENT_CODE.store(code as u32, Ordering::Relaxed);
    CURRENT_MODS.store(carbon_mods as u32, Ordering::Relaxed);
    uninstall_global_monitor();
    unsafe {
        let mask: u64 = 1u64 << KCG_EVENT_KEY_DOWN;
        let tap = CGEventTapCreate(
            KCG_SESSION_EVENT_TAP,
            KCG_HEAD_INSERT_EVENT_TAP,
            KCG_TAP_OPTION_LISTEN_ONLY,
            mask,
            tap_cb,
            ptr::null_mut(),
        );
        if tap.is_null() {
            eprintln!("[hotkey] CGEventTapCreate failed (need '输入监控' 权限?)");
            return;
        }
        let src = CFMachPortCreateRunLoopSource(ptr::null(), tap, 0);
        let rl = CFRunLoopGetMain();
        CFRunLoopAddSource(rl, src, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
        GLOBAL_MONITOR.store(tap, Ordering::SeqCst);
        eprintln!("[hotkey] CGEventTap installed (code={} mods={})", code, carbon_mods);
    }
}

/// 注册全局热键（仅 keyCode；modifiers 固定为 0）。传入负数表示禁用。
pub fn register_hotkey_keycode(key_code: i64) {
    if key_code < 0 {
        unregister_current();
        return;
    }

    ensure_handler_installed();

    unregister_current();
    unsafe {
        let mut hk_ref: EventHotKeyRef = ptr::null_mut();
        // 签名用于识别来源（任意四字节）
        let hotkey_id = EventHotKeyID { signature: ((b'A' as u32) << 24) | ((b'L' as u32) << 16) | ((b'C' as u32) << 8) | (b'Y' as u32), id: 1 };
        let status = RegisterEventHotKey(
            key_code as u32,
            0u32, // 无修饰，简化实现
            hotkey_id,
            GetEventDispatcherTarget(),
            0,
            &mut hk_ref as *mut _,
        );
        eprintln!("[hotkey] register key={} mods=0 status={} (keycode)", key_code, status);
        HOTKEY_REF.store(hk_ref as *mut _, Ordering::SeqCst);
    }
    // 同步安装全局事件监控作为兜底
    install_global_monitor_for_combo(key_code as u16, 0);
}

/// 注册全局热键（支持修饰键）。
pub fn register_hotkey_combo(key_code: i64, modifiers: u32) {
    if key_code < 0 {
        unregister_current();
        return;
    }

    ensure_handler_installed();

    unregister_current();
    uninstall_global_monitor();
    unsafe {
        let mut hk_ref: EventHotKeyRef = ptr::null_mut();
        let hotkey_id = EventHotKeyID { signature: ((b'A' as u32) << 24) | ((b'L' as u32) << 16) | ((b'C' as u32) << 8) | (b'Y' as u32), id: 1 };
        let status = RegisterEventHotKey(
            key_code as u32,
            modifiers,
            hotkey_id,
            GetEventDispatcherTarget(),
            0,
            &mut hk_ref as *mut _,
        );
        eprintln!("[hotkey] register key={} mods={} status={}", key_code, modifiers, status);
        HOTKEY_REF.store(hk_ref as *mut _, Ordering::SeqCst);
    }
    // 安装全局监控兜底（后台也能收到）
    install_global_monitor_for_combo(key_code as u16, modifiers);
}

// NS -> Carbon 修饰位映射
const NS_MOD_SHIFT: u64 = 1 << 17;
const NS_MOD_CTRL: u64 = 1 << 18;
const NS_MOD_ALT: u64 = 1 << 19; // Option
const NS_MOD_CMD: u64 = 1 << 20;

// Carbon 位：cmdKey, shiftKey, optionKey, controlKey
const CARBON_SHIFT: u32 = 1 << 9;  // 0x0200
const CARBON_CMD: u32 = 1 << 8;    // 0x0100
const CARBON_ALT: u32 = 1 << 11;   // 0x0800
const CARBON_CTRL: u32 = 1 << 12;  // 0x1000

pub fn nsflags_to_carbon_modifiers(ns: u64) -> u32 {
    let mut m = 0u32;
    if ns & NS_MOD_CMD != 0 { m |= CARBON_CMD; }
    if ns & NS_MOD_SHIFT != 0 { m |= CARBON_SHIFT; }
    if ns & NS_MOD_ALT != 0 { m |= CARBON_ALT; }
    if ns & NS_MOD_CTRL != 0 { m |= CARBON_CTRL; }
    m
}

// 把 F1..F19 的标题映射到 macOS 虚拟键码
pub fn fkey_title_to_keycode(title: &str) -> Option<i64> {
    match title.trim() {
        "F1" => Some(122),
        "F2" => Some(120),
        "F3" => Some(99),
        "F4" => Some(118),
        "F5" => Some(96),
        "F6" => Some(97),
        "F7" => Some(98),
        "F8" => Some(100),
        "F9" => Some(101),
        "F10" => Some(109),
        "F11" => Some(103),
        "F12" => Some(111),
        "F13" => Some(105),
        "F14" => Some(107),
        "F15" => Some(113),
        "F16" => Some(106),
        "F17" => Some(64),
        "F18" => Some(79),
        "F19" => Some(80),
        _ => None,
    }
}

/// 从偏好初始化（无值则禁用）。
pub fn init_from_prefs() {
    let code = super::status_bar::get_saved_hotkey_code();
    let mods = super::status_bar::get_saved_hotkey_modifiers() as u32;
    if code >= 0 { register_hotkey_combo(code, mods); } else { unregister_current(); }
}
