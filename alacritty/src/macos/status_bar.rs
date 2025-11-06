use objc2::{MainThreadMarker, class, msg_send, sel};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2_foundation::NSString;
use objc2_app_kit::{NSApplication, NSStatusBar, NSStatusItem, NSMenu, NSMenuItem};
use std::collections::HashMap;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::cell::RefCell;
use std::sync::OnceLock;
use winit::event_loop::EventLoopProxy;

use crate::cli::WindowOptions;
use crate::event::{Event, EventType};

// 全局保存指针（原生指针是线程安全可共享的）。
// 兼容旧实现的全局指针（不再作为逻辑依据，仅做向后兼容）。
static STATUS_ITEM_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
static NSWINDOW_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
static MENU_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
static EVENT_PROXY: OnceLock<EventLoopProxy<Event>> = OnceLock::new();
// 记录所有已创建的 NSWindow 指针，用于统一显示/隐藏。
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

// 每个窗口的状态栏项记录
struct PerWindowStatus {
    status_item: *mut AnyObject,
    menu: *mut AnyObject,
    ns_window: *mut AnyObject,
}

// handler(this) -> PerWindowStatus 的映射（仅在主线程访问）。
thread_local! {
    static HANDLER_MAP: RefCell<HashMap<*mut AnyObject, PerWindowStatus>> = RefCell::new(HashMap::new());
}

// 递增编号用于默认的每窗口标题，例如“窗口1/窗口2 …”。
static NEXT_INDEX: AtomicUsize = AtomicUsize::new(1);

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

        extern "C" fn on_click(this: &AnyObject, _sel: Sel, _sender: *mut AnyObject) {
            // 根据当前事件类型判断是否为右键
            let mut handled_right = false;
            unsafe {
                let app: *mut NSApplication = msg_send![class!(NSApplication), sharedApplication];
                let ev: *mut AnyObject = msg_send![app, currentEvent];
                if !ev.is_null() {
                    let btn_num: i64 = msg_send![ev, buttonNumber];
                    if btn_num == 1 { // 右键
                        // 右键：针对当前 handler 对应的窗口弹出其独立菜单
                        let this_ptr = (this as *const _ as *mut AnyObject);
                        let (menu_ptr, item_ptr) = HANDLER_MAP.with(|map| {
                            let map = map.borrow();
                            match map.get(&this_ptr) {
                                Some(rec) => (rec.menu, rec.status_item),
                                None => (std::ptr::null_mut(), std::ptr::null_mut()),
                            }
                        });
                        if !menu_ptr.is_null() && !item_ptr.is_null() {
                            let _: () = msg_send![item_ptr, popUpStatusItemMenu: menu_ptr];
                            handled_right = true;
                        }
                    }
                }
            }
            if handled_right { return; }
            // 左键：切换“当前 handler 对应窗口”的显示/隐藏；若无绑定窗口则切换全部。
            let this_ptr = (this as *const _ as *mut AnyObject);
            let ns_win = HANDLER_MAP.with(|map| map.borrow().get(&this_ptr).map(|r| r.ns_window));
            if let Some(win) = ns_win {
                if !win.is_null() { toggle_specific_window(win); return; }
            }
            // 兜底：若找不到对应窗口，则触发“切换全部窗口”。
            if let Some(proxy) = EVENT_PROXY.get() {
                let _ = proxy.send_event(Event::new(EventType::ToggleAllWindows, None));
            }
        }

        extern "C" fn on_new_window(_this: &AnyObject, _sel: Sel, _sender: *mut AnyObject) {
            // 通过事件代理请求创建新窗口；随后无条件显示所有窗口。
            if let Some(proxy) = EVENT_PROXY.get() {
                let _ = proxy.send_event(Event::new(
                    EventType::CreateWindow(WindowOptions::default()),
                    None,
                ));
                let _ = proxy.send_event(Event::new(EventType::ShowAllWindows, None));
            }
        }

        unsafe {
            builder.add_method(sel!(onStatusItemClick:), on_click as extern "C" fn(_, _, _));
            builder.add_method(sel!(onStatusItemNewWindow:), on_new_window as extern "C" fn(_, _, _));
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

    // 默认返回第一个状态栏项的锚点（主要用于已有实现的定位）。
    // 为简化，此处沿用历史全局指针；若未设置则返回 None。
    let item = STATUS_ITEM_PTR.load(Ordering::Relaxed);
    if item.is_null() { return None; }
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

fn toggle_specific_window(win: *mut AnyObject) {
    if win.is_null() { return; }
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
    // 为“每个窗口”创建独立的状态栏项与菜单，并绑定点击事件。
    create_status_item_for_window(ns_window, Some("Alacritty"));
}

/// 创建或复用右键菜单，并设置目标对象。
fn build_context_menu_for_target(target: *mut AnyObject) -> *mut AnyObject {
    unsafe {
        // 创建菜单
        let menu: *mut AnyObject = msg_send![class!(NSMenu), new];

        // 新增窗口菜单项
        let title = NSString::from_str("新增窗口");
        let empty_key = NSString::from_str("");
        let mi_alloc: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
        let mi: *mut AnyObject = msg_send![
            mi_alloc,
            initWithTitle: &*title,
            action: sel!(onStatusItemNewWindow:),
            keyEquivalent: &*empty_key
        ];
        let _: () = msg_send![mi, setTarget: target];
        let _: () = msg_send![menu, addItem: mi];

        menu
    }
}

/// 提供事件代理给状态栏菜单使用（用于“新增窗口”）。
pub fn set_event_proxy(proxy: EventLoopProxy<Event>) {
    let _ = EVENT_PROXY.set(proxy);
}

// 显示/隐藏的统一实现已移动至 `display/window.rs`，这里不再持有窗口列表。

/// 为指定 NSWindow 创建一个独立的状态栏项与菜单，并绑定事件。
pub fn create_status_item_for_window(ns_window: *mut AnyObject, title: Option<&str>) {
    assert!(MainThreadMarker::new().is_some());
    let _ = BORDER_STYLE.get_or_init(parse_border_style_from_env);

    // 创建状态栏项
    let bar = NSStatusBar::systemStatusBar();
    let item: Retained<NSStatusItem> = bar.statusItemWithLength(-1.0);

    // 默认标题：窗口N
    let label = if let Some(t) = title { t.to_string() } else {
        let idx = NEXT_INDEX.fetch_add(1, Ordering::Relaxed);
        format!("窗口{idx}")
    };
    let title_ns = NSString::from_str(&label);
    item.setTitle(Some(&title_ns));

    // 创建 handler 并绑定 action
    let cls = ensure_click_handler_class();
    let handler: Retained<AnyObject> = unsafe { msg_send![cls, new] };

    unsafe {
        let btn: *mut AnyObject = msg_send![&*item, button];
        if !btn.is_null() {
            let _: () = msg_send![btn, setTarget: &*handler];
            let _: () = msg_send![btn, setAction: sel!(onStatusItemClick:)];
            // 左键/右键抬起都触发 action
            let left_up_mask: u64 = 1u64 << 2;
            let right_up_mask: u64 = 1u64 << 4;
            let mask = left_up_mask | right_up_mask;
            let _: u64 = msg_send![btn, sendActionOn: mask];
        } else {
            // 旧 API 回退
            let _: () = msg_send![&*item, setTarget: &*handler];
            let _: () = msg_send![&*item, setAction: sel!(onStatusItemClick:)];
        }
    }

    // 为该 handler 构建独立菜单
    let menu = build_context_menu_for_target((&*handler) as *const _ as *mut AnyObject);

    // 建立映射：handler -> {item, menu, window}
    let item_ptr: *mut AnyObject = (&*item) as *const _ as *mut AnyObject;
    let handler_ptr: *mut AnyObject = (&*handler) as *const _ as *mut AnyObject;
    HANDLER_MAP.with(|map| {
        map.borrow_mut().insert(
            handler_ptr,
            PerWindowStatus { status_item: item_ptr, menu, ns_window },
        );
    });

    // 保持对象存活（简单处理：泄漏到进程结束）
    std::mem::forget(item);
    std::mem::forget(handler);
}

/// 创建一个全局主状态栏项，用于在无窗口时也可新建窗口或切换全部窗口。
pub fn create_global_status_item(title: &str) {
    assert!(MainThreadMarker::new().is_some());
    let _ = BORDER_STYLE.get_or_init(parse_border_style_from_env);

    let bar = NSStatusBar::systemStatusBar();
    let item: Retained<NSStatusItem> = bar.statusItemWithLength(-1.0);

    let title_ns = NSString::from_str(title);
    item.setTitle(Some(&title_ns));

    // 处理器
    let cls = ensure_click_handler_class();
    let handler: Retained<AnyObject> = unsafe { msg_send![cls, new] };

    unsafe {
        let btn: *mut AnyObject = msg_send![&*item, button];
        if !btn.is_null() {
            let _: () = msg_send![btn, setTarget: &*handler];
            let _: () = msg_send![btn, setAction: sel!(onStatusItemClick:)];
            let left_up_mask: u64 = 1u64 << 2;
            let right_up_mask: u64 = 1u64 << 4;
            let mask = left_up_mask | right_up_mask;
            let _: u64 = msg_send![btn, sendActionOn: mask];
        } else {
            let _: () = msg_send![&*item, setTarget: &*handler];
            let _: () = msg_send![&*item, setAction: sel!(onStatusItemClick:)];
        }
    }

    // 上下文菜单
    let menu = build_context_menu_for_target((&*handler) as *const _ as *mut AnyObject);

    // 建立映射（无绑定窗口）
    let item_ptr: *mut AnyObject = (&*item) as *const _ as *mut AnyObject;
    let handler_ptr: *mut AnyObject = (&*handler) as *const _ as *mut AnyObject;
    HANDLER_MAP.with(|map| {
        map.borrow_mut().insert(
            handler_ptr,
            PerWindowStatus { status_item: item_ptr, menu, ns_window: std::ptr::null_mut() },
        );
    });

    // 兼容旧全局指针（用于可能的锚点/回退）
    STATUS_ITEM_PTR.store(item_ptr, Ordering::Relaxed);
    MENU_PTR.store(menu, Ordering::Relaxed);

    std::mem::forget(item);
    std::mem::forget(handler);
}
