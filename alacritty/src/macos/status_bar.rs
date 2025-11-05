use objc2::{MainThreadMarker, class, msg_send, sel};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2_foundation::NSString;
use objc2_app_kit::{NSApplication, NSStatusBar, NSStatusItem};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::OnceLock;

// 全局保存指针（原生指针是线程安全可共享的）。
static STATUS_ITEM_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
static NSWINDOW_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
// 记录我们创建的标题栏视图，避免重复创建（可为空）。
//

#[derive(Copy, Clone, Debug)]
pub struct PopupBorderStyle {
    pub width: f32,
    pub color: (u8, u8, u8),
    pub alpha: f32,
    pub radius: f64,
    pub shadow: bool,
}

static BORDER_STYLE: OnceLock<PopupBorderStyle> = OnceLock::new();

fn parse_border_style_from_env() -> PopupBorderStyle {
    let mut style = PopupBorderStyle { width: 2.0, color: (0, 0, 0), alpha: 0.4, radius: 8.0, shadow: true };
    if let Ok(s) = std::env::var("ALACRITTY_POPUP_BORDER") {
        for part in s.split(',') {
            let mut it = part.splitn(2, '=');
            let k = it.next().unwrap_or("").trim().to_ascii_lowercase();
            let v = it.next().unwrap_or("").trim();
            match k.as_str() {
                "width" | "w" => {
                    if let Ok(f) = v.parse::<f32>() { style.width = f.max(0.0); }
                },
                "alpha" | "a" => {
                    if let Ok(f) = v.parse::<f32>() { style.alpha = f.clamp(0.0, 1.0); }
                },
                "radius" | "r" => {
                    if let Ok(f) = v.parse::<f64>() { style.radius = f.max(0.0); }
                },
                "shadow" | "s" => {
                    let lv = v.to_ascii_lowercase();
                    style.shadow = matches!(lv.as_str(), "1" | "true" | "yes" | "y" | "on");
                },
                "color" | "c" => {
                    let hex = v.trim_start_matches('#');
                    if hex.len() == 6 {
                        if let (Ok(r), Ok(g), Ok(b)) = (
                            u8::from_str_radix(&hex[0..2], 16),
                            u8::from_str_radix(&hex[2..4], 16),
                            u8::from_str_radix(&hex[4..6], 16),
                        ) {
                            style.color = (r, g, b);
                        }
                    }
                },
                _ => {},
            }
        }
    }
    style
}

pub fn border_style() -> PopupBorderStyle {
    *BORDER_STYLE.get_or_init(parse_border_style_from_env)
}

// 动态注册一个 Objective-C 类，作为 target/action 的处理对象。
fn ensure_click_handler_class() -> &'static AnyClass {
    use objc2::declare::ClassBuilder;
    use std::ffi::CString;

    // 单次注册
    static mut CLS: Option<&'static AnyClass> = None;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let name = CString::new("AlacrittyStatusClickHandler").unwrap();
        let mut builder = ClassBuilder::new(name.as_c_str(), class!(NSObject))
            .expect("create class builder");

        extern "C" fn on_click(_this: &AnyObject, _sel: Sel, sender: *mut AnyObject) {
            toggle_main_window(sender);
        }

        unsafe {
            builder.add_method(sel!(onStatusItemClick:), on_click as extern "C" fn(_, _, _));
        }

        let cls = builder.register();
        CLS = Some(cls);
    });

    unsafe { CLS.unwrap() }
}


fn configure_popup_window(ns_win: *mut AnyObject) {
    unsafe {
        // 使用系统标题栏（可见），避免“看起来被删除”
        if msg_send![ns_win, respondsToSelector: sel!(setTitlebarAppearsTransparent:)] {
            let _: () = msg_send![ns_win, setTitlebarAppearsTransparent: false];
        }
        if msg_send![ns_win, respondsToSelector: sel!(setTitleVisibility:)] {
            let _: () = msg_send![ns_win, setTitleVisibility: 0u64 /* NSWindowTitleVisible */];
        }
        if msg_send![ns_win, respondsToSelector: sel!(styleMask)]
            && msg_send![ns_win, respondsToSelector: sel!(setStyleMask:)]
        {
            let mask: u64 = msg_send![ns_win, styleMask];
            let fullsize_bit: u64 = 1u64 << 15; // NSWindowStyleMaskFullSizeContentView
            let cleared = mask & !fullsize_bit; // 不让内容延伸到标题栏
            let _: () = msg_send![ns_win, setStyleMask: cleared];
        }
        // 仅标题栏可拖动
        if msg_send![ns_win, respondsToSelector: sel!(setMovableByWindowBackground:)] {
            let _: () = msg_send![ns_win, setMovableByWindowBackground: false];
        }

        // 边框改由渲染层绘制；此处不再调用 setContentBorderThickness，避免潜在兼容性问题。

        // 隐藏标准按钮（关闭、最小化、缩放）
        for i in 0u64..=2u64 {
            let btn: *mut AnyObject = msg_send![ns_win, standardWindowButton: i];
            if !btn.is_null() {
                let _: () = msg_send![btn, setHidden: true];
                let _: () = msg_send![btn, setEnabled: false];
            }
        }

        // 设置圆角与阴影（安全调用）
        let cv: *mut AnyObject = msg_send![ns_win, contentView];
        if !cv.is_null() {
            let _: () = msg_send![cv, setWantsLayer: true];
            let layer: *mut AnyObject = msg_send![cv, layer];
            if !layer.is_null() {
                // 顶部左右直角：不对内容视图应用圆角
                let _: () = msg_send![layer, setCornerRadius: 0.0f64];
                let _: () = msg_send![layer, setMasksToBounds: false];
            }

        }
        if msg_send![ns_win, respondsToSelector: sel!(setHasShadow:)] {
            let style = border_style();
            let _: () = msg_send![ns_win, setHasShadow: style.shadow];
        }

        // 确保窗口在“当前桌面/Space”显示。
        // 通过设置 NSWindowCollectionBehaviorMoveToActiveSpace | NSWindowCollectionBehaviorTransient。
        // 位定义参考 AppKit：
        //  - MoveToActiveSpace = 1 << 1
        //  - Transient          = 1 << 3
        if msg_send![ns_win, respondsToSelector: sel!(setCollectionBehavior:)]
            && msg_send![ns_win, respondsToSelector: sel!(collectionBehavior)]
        {
            let existing: u64 = msg_send![ns_win, collectionBehavior];
            let move_to_active_space: u64 = 1u64 << 1;
            let transient: u64 = 1u64 << 3;
            let combined = existing | move_to_active_space | transient;
            let _: () = msg_send![ns_win, setCollectionBehavior: combined];
        }
    }
}

/// 计算状态栏按钮的锚点（按钮窗口中心 X 与窗口底边 Y）。
/// 用于在 Rust/winit 侧自行定位窗口。
pub fn status_item_anchor() -> Option<(f64, f64)> {
    assert!(MainThreadMarker::new().is_some());

    let item = STATUS_ITEM_PTR.load(Ordering::Relaxed);
    if item.is_null() {
        return None;
    }

    unsafe {
        let btn: *mut AnyObject = msg_send![item, button];
        if btn.is_null() {
            return None;
        }

        let kx = NSString::from_str("window.frame.origin.x");
        let kw = NSString::from_str("window.frame.size.width");
        let ky = NSString::from_str("window.frame.origin.y");

        let x_num: *mut AnyObject = msg_send![btn, valueForKeyPath: (&*kx) as *const _ as *mut AnyObject];
        let w_num: *mut AnyObject = msg_send![btn, valueForKeyPath: (&*kw) as *const _ as *mut AnyObject];
        let y_num: *mut AnyObject = msg_send![btn, valueForKeyPath: (&*ky) as *const _ as *mut AnyObject];
        if x_num.is_null() || w_num.is_null() || y_num.is_null() { return None; }

        let x: f64 = msg_send![x_num, doubleValue];
        let w: f64 = msg_send![w_num, doubleValue];
        let y: f64 = msg_send![y_num, doubleValue];

        Some((x + w / 2.0, y))
    }
}

//

fn toggle_main_window(sender: *mut AnyObject) {
    let win = NSWINDOW_PTR.load(Ordering::Relaxed);
    if win.is_null() {
        return;
    }

    unsafe {
        let visible: bool = msg_send![win, isVisible];
        if visible {
            let _: () = msg_send![win, orderOut: std::ptr::null::<AnyObject>()];
        } else {
            configure_popup_window(win);
            // 先激活应用，再显示窗口
            let app: *mut NSApplication = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, activateIgnoringOtherApps: true];
            let _: () = msg_send![win, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
        }
    }
}

/// 初始化并显示状态栏（菜单栏）文字。
/// 多次调用将更新现有文字。
pub fn init_status_bar_text(text: &str) {
    assert!(MainThreadMarker::new().is_some());
    let _ = BORDER_STYLE.get_or_init(parse_border_style_from_env);
    let bar = NSStatusBar::systemStatusBar();
    // -1.0 等同于 NSVariableStatusItemLength，使用自适应长度
    let item: Retained<NSStatusItem> = bar.statusItemWithLength(-1.0);

    let title = NSString::from_str(text);
    // 直接设置 NSStatusItem 的 title（AppKit 建议用 button.title，但该绑定版本尚无 button 方法）
    item.setTitle(Some(&title));

    // 防止被释放：让其泄漏到进程生命周期结束（简单可靠）
    let raw: *mut AnyObject = (&*item) as *const _ as *mut AnyObject;
    STATUS_ITEM_PTR.store(raw, Ordering::Relaxed);
    std::mem::forget(item);
}

/// 绑定菜单栏点击事件以切换窗口显示/隐藏。
/// 需在创建好 winit 窗口后调用，并传入其 NSWindow 指针。
pub fn bind_toggle_to_window(ns_window: *mut AnyObject) {
    assert!(MainThreadMarker::new().is_some());

    NSWINDOW_PTR.store(ns_window, Ordering::Relaxed);

    // 没有状态栏项则无需绑定
    let item = STATUS_ITEM_PTR.load(Ordering::Relaxed);
    if item.is_null() {
        return;
    }

    // 构建点击处理对象并设置为 target/action
    let cls = ensure_click_handler_class();
    let handler: Retained<AnyObject> = unsafe { msg_send![cls, new] };

    unsafe {
        // 将事件绑定到 status item 的 button 上，兼容性更好。
        let btn: *mut AnyObject = msg_send![item, button];
        if !btn.is_null() {
            let _: () = msg_send![btn, setTarget: &*handler];
            let _: () = msg_send![btn, setAction: sel!(onStatusItemClick:)];
        } else {
            // 回退到直接绑定在 item 上（旧 API）
            let _: () = msg_send![item, setTarget: &*handler];
            let _: () = msg_send![item, setAction: sel!(onStatusItemClick:)];
        }
    }

    // 保持 handler 存活至进程结束
    std::mem::forget(handler);
}
