use objc2::runtime::AnyObject;
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSDictionary, NSString, NSUserDefaults, ns_string};

pub mod locale;
pub mod proc;
pub mod status_bar;
pub mod activation_guard;
pub mod hotkey;

pub fn disable_autofill() {
    unsafe {
        NSUserDefaults::standardUserDefaults().registerDefaults(
            &NSDictionary::<NSString, AnyObject>::from_slices(
                &[ns_string!("NSAutoFillHeuristicControllerEnabled")],
                &[ns_string!("NO")],
            ),
        );
    }
}

/// 关闭 macOS 的“自动窗口标签页”特性，防止系统将多个窗口自动合并为标签。
///
/// 相当于 Objective‑C：`[NSWindow setAllowsAutomaticWindowTabbing:NO];`
/// 需要在创建任何窗口之前调用。
#[inline]
pub fn disable_automatic_window_tabbing() {
    unsafe {
        let cls = class!(NSWindow);
        // 仅在系统支持该类方法时调用，避免在早期系统崩溃。
        if msg_send![cls, respondsToSelector: sel!(setAllowsAutomaticWindowTabbing:)] {
            let _: () = msg_send![cls, setAllowsAutomaticWindowTabbing: false];
        }
    }
}
