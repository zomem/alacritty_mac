use objc2::{MainThreadMarker, class, msg_send, sel};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Sel, Bool};
use objc2_foundation::{NSString, NSRect, NSPoint, NSSize, NSUserDefaults};
use objc2_app_kit::{NSApplication, NSStatusBar, NSStatusItem, NSMenu, NSMenuItem};
use crate::macos::hotkey;
use std::collections::HashMap;
use std::sync::atomic::{AtomicPtr, AtomicUsize, AtomicIsize, Ordering};
use std::sync::atomic::AtomicBool;
use std::cell::RefCell;
use std::sync::OnceLock;
use winit::event_loop::EventLoopProxy;

use crate::cli::WindowOptions;
use crate::event::{Event, EventType};
use std::path::PathBuf;
use std::path::Path;
use std::fs;

use toml_edit::{DocumentMut, Item, Array as TomlArray};

// 全局保存指针（原生指针是线程安全可共享的）。
// 兼容旧实现的全局指针（不再作为逻辑依据，仅做向后兼容）。
static STATUS_ITEM_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
static NSWINDOW_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
static MENU_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
static EVENT_PROXY: OnceLock<EventLoopProxy<Event>> = OnceLock::new();
// 配置窗口与内容视图控件指针
static CONFIG_WINDOW_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
static CONFIG_TABLE_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
// 主题窗口与表格
static THEME_WINDOW_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
static THEME_TABLE_PTR: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
static DRAG_SOURCE_INDEX: AtomicIsize = AtomicIsize::new(-1);
// 防抖：避免在 reloadData 引起的二次通知中重复应用主题
static APPLYING_THEME: AtomicBool = AtomicBool::new(false);
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


fn status_icon_path() -> Option<String> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let try_paths = [
                dir.join("cmd.png"),
                dir.join("../Resources/cmd.png"),
                dir.join("../extra/icons/cmd.png"),
                dir.join("../../extra/icons/cmd.png"),
                dir.join("../../../extra/icons/cmd.png"),
            ];
            for p in try_paths.iter() {
                if p.exists() {
                    return Some(p.display().to_string());
                }
            }
        }
    }

    let fallback = ["extra/icons/cmd.png", "cmd.png"];
    for p in fallback.iter() {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
    }
    None
}

unsafe fn set_status_item_icon(item: &NSStatusItem) -> bool {
    if let Some(path) = status_icon_path() {
        let ns = NSString::from_str(&path);
        let img_alloc: *mut AnyObject = msg_send![class!(NSImage), alloc];
        let img: *mut AnyObject = msg_send![img_alloc, initWithContentsOfFile: &*ns];
        if !img.is_null() {
            // 将图片标记为模板并缩放到菜单栏标准尺寸（pt）。
            // 默认 18pt，可通过环境变量 ALACRITTY_STATUS_ICON_SIZE 覆盖。
            let size_pt: f64 = std::env::var("ALACRITTY_STATUS_ICON_SIZE")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|v| *v > 0.0 && *v < 64.0)
                .unwrap_or(18.0);
            let _: () = msg_send![img, setTemplate: true];
            let _: () = msg_send![img, setSize: NSSize { width: size_pt, height: size_pt }];
            let btn: *mut AnyObject = msg_send![item, button];
            if !btn.is_null() {
                let _: () = msg_send![btn, setImage: img];
                let empty = NSString::from_str("");
                if msg_send![btn, respondsToSelector: sel!(setTitle:)] {
                    let _: () = msg_send![btn, setTitle: &*empty];
                }
                // 确保仅显示图标
                if msg_send![btn, respondsToSelector: sel!(setImagePosition:)] {
                    // NSImageOnly = 2
                    let _: () = msg_send![btn, setImagePosition: 2i64];
                }
            } else {
                let _: () = msg_send![item, setImage: img];
                let empty = NSString::from_str("");
                let _: () = msg_send![item, setTitle: &*empty];
            }
            return true;
        }
    }
    false
}


// ========== 主题处理：路径/读取/写入 工具 ==========
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn expand_tilde<S: AsRef<str>>(p: S) -> String {
    let p = p.as_ref();
    if let Some(home) = home_dir() {
        if let Some(rest) = p.strip_prefix("~") {
            return format!("{}{}", home.display(), rest);
        }
    }
    p.to_string()
}

fn alacritty_config_path() -> Option<PathBuf> {
    // 按需求固定使用 ~/.config/alacritty/alacritty.toml
    let home = home_dir()?;
    Some(home.join(".config").join("alacritty").join("alacritty.toml"))
}

fn theme_dir_path() -> Option<PathBuf> {
    let home = home_dir()?;
    Some(home.join(".config").join("alacritty").join("themes").join("themes"))
}

fn list_theme_files() -> Vec<PathBuf> {
    let dir = match theme_dir_path() { Some(p) => p, None => return vec![] };
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() {
                if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                    if ext.eq_ignore_ascii_case("toml") { out.push(p); }
                }
            }
        }
    }
    out.sort();
    out
}

fn theme_path_to_tilde(path: &Path) -> String {
    // 生成以 ~ 开头的主题路径，固定放在 ~/.config/alacritty/themes/themes 下
    let file = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    format!("~/.config/alacritty/themes/themes/{}", file)
}

fn read_current_theme_expanded() -> Option<String> {
    let cfg = alacritty_config_path()?;
    let data = fs::read_to_string(cfg).ok()?;
    if let Ok(mut doc) = data.parse::<DocumentMut>() {
        let general = &doc["general"];
        if let Item::Table(tbl) = general {
            if let Some(import_item) = tbl.get("import") {
                // 仅取第一个字符串项
                if import_item.is_array() {
                    let arr = import_item.as_array().unwrap();
                    for it in arr.iter() {
                        if let Some(s) = it.as_str() {
                            let expanded = expand_tilde(s);
                            return Some(expanded);
                        }
                    }
                } else if let Some(s) = import_item.as_value().and_then(|v| v.as_str()) {
                    let expanded = expand_tilde(s);
                    return Some(expanded);
                }
            }
        }
    }
    None
}

fn write_theme_to_config(theme_tilde_path: &str) -> Result<(), String> {
    let cfg = alacritty_config_path().ok_or_else(|| "无法定位配置文件路径".to_string())?;
    let mut doc = if let Ok(s) = fs::read_to_string(&cfg) {
        s.parse::<DocumentMut>().map_err(|e| format!("解析配置失败: {e}"))?
    } else {
        DocumentMut::new()
    };

    // 确保 [general]
    if doc.get("general").is_none() {
        doc["general"] = Item::Table(Default::default());
    }
    // 设置 import = [ "..." ]
    let mut arr = TomlArray::default();
    arr.push(theme_tilde_path);
    doc["general"]["import"] = Item::Value(arr.into());

    // 创建父目录
    if let Some(parent) = cfg.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
        }
    }
    fs::write(&cfg, doc.to_string()).map_err(|e| format!("写入配置失败: {e}"))
}

// 主题子菜单已移除，改为独立窗口

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

        // 打开主题窗口
        extern "C" fn on_open_themes(_this: &AnyObject, _sel: Sel, _sender: *mut AnyObject) {
            unsafe { super::status_bar::open_theme_window(); }
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

        extern "C" fn on_open_config(_this: &AnyObject, _sel: Sel, _sender: *mut AnyObject) {
            // 打开配置窗口
            unsafe { super::status_bar::open_config_window(); }
        }

        extern "C" fn on_config_add_path(_this: &AnyObject, _sel: Sel, _sender: *mut AnyObject) {
            // 打开系统文件夹选择对话框，选择文件夹并追加保存
            unsafe { super::status_bar::pick_and_append_folder_path(); }
        }

        // 配置窗口：添加“文本”行（显示在菜单列表顶部，不可点击）
        extern "C" fn on_config_add_text(_this: &AnyObject, _sel: Sel, _sender: *mut AnyObject) {
            unsafe {
                // 使用 NSAlert + accessory NSTextField 询问文本
                let alert: *mut AnyObject = msg_send![class!(NSAlert), alloc];
                let alert: *mut AnyObject = msg_send![alert, init];
                if alert.is_null() { return; }

                let msg = NSString::from_str("添加文本");
                let info = NSString::from_str("输入将显示在菜单栏列表中，且不可点击");
                let _: () = msg_send![alert, setMessageText: &*msg];
                let _: () = msg_send![alert, setInformativeText: &*info];

                // 添加按钮：确定 / 取消（第一个按钮返回 1000）
                let ok = NSString::from_str("确定");
                let cancel = NSString::from_str("取消");
                let _: *mut AnyObject = msg_send![alert, addButtonWithTitle: &*ok];
                let _: *mut AnyObject = msg_send![alert, addButtonWithTitle: &*cancel];

                // 输入框
                let tf: *mut AnyObject = msg_send![class!(NSTextField), alloc];
                let tf: *mut AnyObject = msg_send![
                    tf,
                    initWithFrame: NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: 300.0, height: 22.0 } }
                ];
                let _: () = msg_send![tf, setStringValue: &*NSString::from_str("")];
                let _: () = msg_send![alert, setAccessoryView: tf];

                let resp: i64 = msg_send![alert, runModal];
                if resp != 1000 { return; }

                // 读取文本
                let text_obj: *mut AnyObject = msg_send![tf, stringValue];
                if text_obj.is_null() { return; }
                let c_ptr: *const std::ffi::c_char = msg_send![text_obj, UTF8String];
                if c_ptr.is_null() { return; }
                let mut s = std::ffi::CStr::from_ptr(c_ptr).to_string_lossy().into_owned();
                s = s.trim().to_string();
                if s.is_empty() { return; }
                // 避免重复的前缀：若用户手动输入了 text: 前缀，则去掉
                let s_norm = if let Some(rest) = s.strip_prefix("text:") { rest.trim().to_string() } else { s };

                // 计算插入位置：选中行后插入；若未选中则追加到末尾
                let table = CONFIG_TABLE_PTR.load(Ordering::Relaxed);
                let mut lines: Vec<String> = get_saved_paths_string()
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
                let mut insert_at = lines.len();
                if !table.is_null() {
                    let row: isize = msg_send![table, selectedRow];
                    if row >= 0 {
                        let idx = row as usize;
                        if idx <= lines.len() { insert_at = idx.saturating_add(1); }
                    }
                }
                if insert_at > lines.len() { insert_at = lines.len(); }
                lines.insert(insert_at, format!("text:{}", s_norm));
                set_saved_paths_string(&lines.join("\n"));
                update_config_table();
                rebuild_all_context_menus();
            }
        }

        // 主题列表窗口：点击行切换主题
        extern "C" fn on_theme_row_click(_this: &AnyObject, _sel: Sel, sender: *mut AnyObject) {
            unsafe {
                if sender.is_null() { return; }
                // 优先使用 clickedRow（鼠标点击行），否则回退到 selectedRow
                let mut row: isize = msg_send![sender, clickedRow];
                if row < 0 { row = msg_send![sender, selectedRow]; }
                if row < 0 { return; }
                let idx = row as usize;
                let themes = list_theme_files();
                if idx >= themes.len() { return; }
                if APPLYING_THEME.swap(true, Ordering::SeqCst) { return; }
                let tilde = theme_path_to_tilde(&themes[idx]);
                if let Err(e) = super::status_bar::write_theme_to_config(&tilde) {
                    eprintln!("写入主题到配置失败: {}", e);
                }
                update_theme_table();
                rebuild_all_context_menus();
                APPLYING_THEME.store(false, Ordering::SeqCst);
            }
        }

        // 主题列表窗口：监听选中变化（无论点击还是键盘），立即应用主题
        extern "C" fn on_theme_selection_changed(_this: &AnyObject, _sel: Sel, notif: *mut AnyObject) {
            unsafe {
                if notif.is_null() { return; }
                // 仅处理来自主题表的通知
                let obj: *mut AnyObject = msg_send![notif, object];
                let theme_table = THEME_TABLE_PTR.load(Ordering::Relaxed);
                if obj.is_null() || theme_table.is_null() || obj != theme_table { return; }
                let row: isize = msg_send![theme_table, selectedRow];
                if row < 0 { return; }
                let idx = row as usize;
                let themes = list_theme_files();
                if idx >= themes.len() { return; }
                if APPLYING_THEME.swap(true, Ordering::SeqCst) { return; }
                let tilde = theme_path_to_tilde(&themes[idx]);
                if let Err(e) = super::status_bar::write_theme_to_config(&tilde) {
                    eprintln!("写入主题到配置失败: {}", e);
                }
                update_theme_table();
                rebuild_all_context_menus();
                APPLYING_THEME.store(false, Ordering::SeqCst);
            }
        }

        extern "C" fn on_open_saved_path(_this: &AnyObject, _sel: Sel, sender: *mut AnyObject) {
            // 从菜单项的 representedObject 取出路径字符串，在该目录新建窗口
            unsafe {
                if sender.is_null() { return; }
                let robj: *mut AnyObject = msg_send![sender, representedObject];
                if robj.is_null() { return; }
                let c_ptr: *const std::ffi::c_char = msg_send![robj, UTF8String];
                if c_ptr.is_null() { return; }
                let path = unsafe { std::ffi::CStr::from_ptr(c_ptr) }
                    .to_string_lossy()
                    .into_owned();

                if let Some(proxy) = EVENT_PROXY.get() {
                    let mut opts = WindowOptions::default();
                    opts.terminal_options.working_directory = Some(PathBuf::from(path));
                    let _ = proxy.send_event(Event::new(EventType::CreateWindow(opts), None));
                    let _ = proxy.send_event(Event::new(EventType::ShowAllWindows, None));
                }
            }
        }

        // 配置窗口：录制到组合快捷键
        extern "C" fn on_config_hotkey_recorded(_this: &AnyObject, _sel: Sel, sender: *mut AnyObject) {
            unsafe {
                if sender.is_null() { return; }
                let tag_val: i64 = msg_send![sender, tag];
                // tag: 高32位=mods, 低32位=key_code；-1 表示禁用
                if tag_val < 0 {
                    super::status_bar::set_saved_hotkey_all(-1, 0, "禁用");
                    crate::macos::hotkey::register_hotkey_combo(-1, 0);
                    return;
                }
                let code = (tag_val & 0xFFFF_FFFF) as i64;
                let mods_i = ((tag_val >> 32) & 0xFFFF_FFFF) as i64;
                let text_obj: *mut AnyObject = msg_send![sender, stringValue];
                let display = if !text_obj.is_null() {
                    let c_ptr: *const std::ffi::c_char = msg_send![text_obj, UTF8String];
                    if !c_ptr.is_null() { std::ffi::CStr::from_ptr(c_ptr).to_string_lossy().into_owned() } else { String::new() }
                } else { String::new() };
                super::status_bar::set_saved_hotkey_all(code, mods_i, &display);
                // removed noisy debug print
                hotkey::register_hotkey_combo(code, mods_i as u32);
            }
        }

        // 退出应用
        extern "C" fn on_quit(_this: &AnyObject, _sel: Sel, _sender: *mut AnyObject) {
            unsafe {
                let app: *mut NSApplication = msg_send![class!(NSApplication), sharedApplication];
                let _: () = msg_send![app, terminate: std::ptr::null::<AnyObject>()];
            }
        }

        // NSTableView 数据源/委托 + 配置按钮行为
        extern "C" fn number_of_rows_in_table(_this: &AnyObject, _sel: Sel, table: *mut AnyObject) -> isize {
            let theme_table = THEME_TABLE_PTR.load(Ordering::Relaxed);
            if !theme_table.is_null() && theme_table == table {
                let count = list_theme_files().len();
                return count as isize;
            }
            // 默认：配置窗口的路径列表
            let content = get_saved_paths_string();
            let count = content
                .lines()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .count();
            count as isize
        }

        extern "C" fn table_view_view_for_col_row(
            _this: &AnyObject,
            _sel: Sel,
            table: *mut AnyObject,
            _col: *mut AnyObject,
            row: isize,
        ) -> *mut AnyObject {
            unsafe {
                // Theme 表：按需生成
                let theme_table = THEME_TABLE_PTR.load(Ordering::Relaxed);
                let is_theme = !theme_table.is_null() && theme_table == table;

                let text_str = if is_theme {
                    let themes = list_theme_files();
                    let idx = if row < 0 { 0 } else { row as usize };
                    if idx < themes.len() {
                        themes[idx].file_stem().and_then(|s| s.to_str()).unwrap_or("主题").to_string()
                    } else { String::new() }
                } else {
                    // 配置表：路径文本
                    let lines: Vec<String> = get_saved_paths_string()
                        .lines()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let idx = if row < 0 { 0 } else { row as usize };
                    if idx < lines.len() {
                        let raw = lines[idx].trim();
                        if raw == "---" {
                            "── 分隔线 ──".to_string()
                        } else if let Some(rest) = raw.strip_prefix("text:") {
                            rest.trim().to_string()
                        } else {
                            crate::path_util::shorten_home(raw)
                        }
                    } else {
                        String::new()
                    }
                };

                // 复用/创建容器单元视图：仅左侧文本
                let ident = if is_theme { NSString::from_str("ThemeCell") } else { NSString::from_str("PathCell") };
                let mut cell: *mut AnyObject = msg_send![table, makeViewWithIdentifier: &*ident, owner: table];
                if cell.is_null() {
                    let cell_cls = if is_theme { ensure_theme_cellview_class() } else { ensure_path_cellview_class() };
                    cell = msg_send![cell_cls, alloc];
                    cell = msg_send![cell, initWithFrame: NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: 10.0, height: 10.0 } }];
                    let _: () = msg_send![cell, setIdentifier: &*ident];
                    if msg_send![cell, respondsToSelector: sel!(setAutoresizesSubviews:)] {
                        let _: () = msg_send![cell, setAutoresizesSubviews: true];
                    }

                    // 文本
                    let text: *mut AnyObject = msg_send![class!(NSTextField), alloc];
                    let text: *mut AnyObject = msg_send![text, initWithFrame: NSRect { origin: NSPoint { x: 8.0, y: 0.0 }, size: NSSize { width: 100.0, height: 18.0 } }];
                    let _: () = msg_send![text, setBordered: false];
                    let _: () = msg_send![text, setEditable: false];
                    let _: () = msg_send![text, setBezeled: false];
                    if msg_send![text, respondsToSelector: sel!(setDrawsBackground:)] {
                        let _: () = msg_send![text, setDrawsBackground: false];
                    }
                    if msg_send![text, respondsToSelector: sel!(setUsesSingleLineMode:)] {
                        let _: () = msg_send![text, setUsesSingleLineMode: true];
                    }
                    if !is_theme {
                        // 配置表采用中间省略，主题表由自定义布局控制
                        let trunc_middle: u64 = 5; // NSLineBreakByTruncatingMiddle
                        if msg_send![text, respondsToSelector: sel!(setLineBreakMode:)] {
                            let _: () = msg_send![text, setLineBreakMode: trunc_middle];
                        }
                    }
                    // 左对齐文本
                    let align_left: i64 = 0; // NSTextAlignmentLeft
                    if msg_send![text, respondsToSelector: sel!(setAlignment:)] {
                        let _: () = msg_send![text, setAlignment: align_left];
                    }
                    if msg_send![text, respondsToSelector: sel!(setSelectable:)] {
                        let _: () = msg_send![text, setSelectable: false];
                    }
                    let tag = if is_theme { 2101isize } else { 1002isize };
                    let _: () = msg_send![text, setTag: tag];
                    let _: () = msg_send![cell, addSubview: text];

                    if is_theme {
                        // 右侧勾标记（默认隐藏，选中主题时显示）
                        let check: *mut AnyObject = msg_send![class!(NSTextField), alloc];
                        let check: *mut AnyObject = msg_send![check, initWithFrame: NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: 16.0, height: 18.0 } }];
                        let tick = NSString::from_str("✓");
                        let _: () = msg_send![check, setStringValue: &*tick];
                        let _: () = msg_send![check, setBordered: false];
                        let _: () = msg_send![check, setEditable: false];
                        let _: () = msg_send![check, setBezeled: false];
                        if msg_send![check, respondsToSelector: sel!(setDrawsBackground:)] { let _: () = msg_send![check, setDrawsBackground: false]; }
                        let align_center: i64 = 2; // NSTextAlignmentCenter
                        if msg_send![check, respondsToSelector: sel!(setAlignment:)] { let _: () = msg_send![check, setAlignment: align_center]; }
                        if msg_send![check, respondsToSelector: sel!(setSelectable:)] { let _: () = msg_send![check, setSelectable: false]; }
                        let _: () = msg_send![check, setHidden: true];
                        let _: () = msg_send![check, setTag: 2102isize];
                        let _: () = msg_send![cell, addSubview: check];
                    }
                }

                // 更新内容，布局交由自定义 CellView 处理
                let text_tag = if is_theme { 2101isize } else { 1002isize };
                let text: *mut AnyObject = msg_send![cell, viewWithTag: text_tag];
                if !text.is_null() {
                    let ns = NSString::from_str(&text_str);
                    let _: () = msg_send![text, setStringValue: &*ns];
                }
                if is_theme {
                    let check: *mut AnyObject = msg_send![cell, viewWithTag: 2102isize];
                    if !check.is_null() {
                        let themes = list_theme_files();
                        let idx = if row < 0 { 0 } else { row as usize };
                        let is_current = if idx < themes.len() {
                            let tilde = theme_path_to_tilde(&themes[idx]);
                            read_current_theme_expanded().map(|c| c == expand_tilde(&tilde)).unwrap_or(false)
                        } else { false };
                        let _: () = msg_send![check, setHidden: !is_current];
                    }
                }
                if msg_send![cell, respondsToSelector: sel!(setNeedsLayout:)] {
                    let _: () = msg_send![cell, setNeedsLayout: true];
                }

                cell
            }
        }

        extern "C" fn on_row_delete(_this: &AnyObject, _sel: Sel, sender: *mut AnyObject) {
            unsafe {
                let table = CONFIG_TABLE_PTR.load(Ordering::Relaxed);
                if table.is_null() { return; }
                // 通过 NSTableView 计算该视图所在行
                let row: isize = msg_send![table, rowForView: sender];
                // removed noisy debug print
                if row < 0 { return; }
                let mut lines: Vec<String> = get_saved_paths_string()
                    .lines()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let idx = row as usize;
                if idx >= lines.len() { return; }
                lines.remove(idx);
                set_saved_paths_string(&lines.join("\n"));
                update_config_table();
                rebuild_all_context_menus();
            }
        }

        // 底部“－”按钮：按选中行移除
        extern "C" fn on_config_remove_selected(_this: &AnyObject, _sel: Sel, _sender: *mut AnyObject) {
            unsafe {
                let table = CONFIG_TABLE_PTR.load(Ordering::Relaxed);
                if table.is_null() { return; }
                let row: isize = msg_send![table, selectedRow];
                if row < 0 { return; }
                let mut lines: Vec<String> = get_saved_paths_string()
                    .lines()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let idx = row as usize;
                if idx >= lines.len() { return; }
                lines.remove(idx);
                set_saved_paths_string(&lines.join("\n"));
                update_config_table();
                rebuild_all_context_menus();
            }
        }

        // 在选中行后插入分隔线（---），若未选中则追加到末尾
        extern "C" fn on_config_add_separator(_this: &AnyObject, _sel: Sel, _sender: *mut AnyObject) {
            unsafe {
                let table = CONFIG_TABLE_PTR.load(Ordering::Relaxed);
                let mut lines: Vec<String> = get_saved_paths_string()
                    .lines()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();

                let mut insert_at = lines.len();
                if !table.is_null() {
                    let row: isize = msg_send![table, selectedRow];
                    if row >= 0 {
                        let idx = row as usize;
                        if idx <= lines.len() { insert_at = idx.saturating_add(1); }
                    }
                }
                if insert_at > lines.len() { insert_at = lines.len(); }
                lines.insert(insert_at, "---".to_string());
                set_saved_paths_string(&lines.join("\n"));
                update_config_table();
                rebuild_all_context_menus();
            }
        }

        // 拖拽排序：整行可拖拽
        extern "C" fn table_view_write_rows(
            _this: &AnyObject,
            _sel: Sel,
            table: *mut AnyObject,
            index_set: *mut AnyObject,
            pb: *mut AnyObject,
        ) -> Bool {
            // 仅对配置表支持拖拽；主题表返回 NO
            let theme_table = THEME_TABLE_PTR.load(Ordering::Relaxed);
            if !theme_table.is_null() && theme_table == table { return Bool::NO; }
            unsafe {
                let first: u64 = msg_send![index_set, firstIndex];
                let row = first as isize;
                // removed noisy debug print
                // 为拖拽声明粘贴板类型并写入占位数据（本地拖拽也需要）
                if !pb.is_null() {
                    let drag_type = NSString::from_str("com.alacritty.pathrow");
                    let types: *mut AnyObject = msg_send![class!(NSArray), arrayWithObject: &*drag_type];
                    let _: isize = msg_send![pb, declareTypes: types, owner: std::ptr::null::<AnyObject>()];
                    let payload = NSString::from_str("row");
                    let _: Bool = msg_send![pb, setString: &*payload, forType: &*drag_type];
                }
                DRAG_SOURCE_INDEX.store(row, Ordering::Relaxed);
            }
            Bool::YES
        }

        extern "C" fn table_view_validate_drop(
            _this: &AnyObject,
            _sel: Sel,
            table: *mut AnyObject,
            _info: *mut AnyObject,
            row: isize,
            _op: isize,
        ) -> u64 {
            // 仅对配置表支持拖拽；主题表返回 0
            let theme_table = THEME_TABLE_PTR.load(Ordering::Relaxed);
            if !theme_table.is_null() && theme_table == table { return 0; }
            unsafe {
                let drop_above: i64 = 1; // NSTableViewDropAbove
                let _: () = msg_send![table, setDropRow: row, dropOperation: drop_above];
            }
            // removed noisy debug print
            16 // NSDragOperationMove
        }

        extern "C" fn table_view_accept_drop(
            _this: &AnyObject,
            _sel: Sel,
            table: *mut AnyObject,
            _info: *mut AnyObject,
            row: isize,
            _op: isize,
        ) -> Bool {
            // 仅对配置表支持拖拽；主题表返回 NO
            let theme_table = THEME_TABLE_PTR.load(Ordering::Relaxed);
            if !theme_table.is_null() && theme_table == table { return Bool::NO; }
            unsafe {
                let from = DRAG_SOURCE_INDEX.swap(-1, Ordering::Relaxed);
                if from < 0 { return Bool::NO; }
                // removed noisy debug print
                let mut lines: Vec<String> = get_saved_paths_string()
                    .lines()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if lines.is_empty() { return Bool::NO; }
                let len = lines.len();
                let mut to = row.max(0) as usize;
                if to > len { to = len; }
                let from_us = from as usize;
                if from_us >= len { return Bool::NO; }
                let item = lines.remove(from_us);
                if from_us < to { to = to.saturating_sub(1); }
                if to > lines.len() { to = lines.len(); }
                lines.insert(to, item);
                set_saved_paths_string(&lines.join("\n"));
                update_config_table();
                rebuild_all_context_menus();
            }
            Bool::YES
        }

        unsafe {
            builder.add_method(sel!(onStatusItemClick:), on_click as extern "C" fn(_, _, _));
            builder.add_method(sel!(onStatusItemNewWindow:), on_new_window as extern "C" fn(_, _, _));
            builder.add_method(sel!(onStatusItemOpenConfig:), on_open_config as extern "C" fn(_, _, _));
            builder.add_method(sel!(onConfigAddPath:), on_config_add_path as extern "C" fn(_, _, _));
            builder.add_method(sel!(onStatusItemOpenSavedPath:), on_open_saved_path as extern "C" fn(_, _, _));
            builder.add_method(sel!(onStatusItemQuit:), on_quit as extern "C" fn(_, _, _));
            builder.add_method(sel!(onConfigHotkeyRecorded:), on_config_hotkey_recorded as extern "C" fn(_, _, _));
            // 主题窗口
            builder.add_method(sel!(onStatusItemOpenThemes:), on_open_themes as extern "C" fn(_, _, _));

            // 表格数据源/委托
            builder.add_method(sel!(numberOfRowsInTableView:), number_of_rows_in_table as extern "C" fn(_, _, _) -> isize);
            builder.add_method(sel!(tableView:viewForTableColumn:row:), table_view_view_for_col_row as extern "C" fn(_, _, _, _, isize) -> *mut AnyObject);
            // 拖拽 & 行按钮
            builder.add_method(sel!(tableView:writeRowsWithIndexes:toPasteboard:), table_view_write_rows as extern "C" fn(_, _, _, _, _) -> Bool);
            builder.add_method(sel!(tableView:validateDrop:proposedRow:proposedDropOperation:), table_view_validate_drop as extern "C" fn(_, _, _, _, isize, isize) -> u64);
            builder.add_method(sel!(tableView:acceptDrop:row:dropOperation:), table_view_accept_drop as extern "C" fn(_, _, _, _, isize, isize) -> Bool);
            builder.add_method(sel!(onRowDelete:), on_row_delete as extern "C" fn(_, _, _));
            builder.add_method(sel!(onConfigRemoveSelected:), on_config_remove_selected as extern "C" fn(_, _, _));
            builder.add_method(sel!(onConfigAddSeparator:), on_config_add_separator as extern "C" fn(_, _, _));
            builder.add_method(sel!(onConfigAddText:), on_config_add_text as extern "C" fn(_, _, _));
            builder.add_method(sel!(onThemeRowClick:), on_theme_row_click as extern "C" fn(_, _, _));
            builder.add_method(sel!(onThemeSelectionChanged:), on_theme_selection_changed as extern "C" fn(_, _, _));
        }

        let cls = builder.register();
        CLS = Some(cls);
    });

    unsafe { CLS.unwrap() }
}

// 自定义 NSTableView 子类：统一在表格区域显示“小手”光标
fn ensure_path_tableview_class() -> &'static AnyClass {
    use objc2::declare::ClassBuilder;
    use std::ffi::CString;

    static mut CLS: Option<&'static AnyClass> = None;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let name = CString::new("AlacrittyPathTableView").unwrap();
        let mut builder = ClassBuilder::new(name.as_c_str(), class!(NSTableView))
            .expect("create table view subclass");

        extern "C" fn reset_cursor_rects(this: &AnyObject, _sel: Sel) {
            unsafe {
                // 在整行（保留少量右侧 padding）范围内使用 openHand 光标
                let bounds: NSRect = msg_send![this, bounds];
                let right_pad: f64 = 4.0;
                let width = (bounds.size.width - right_pad).max(1.0);
                let rect = NSRect { origin: bounds.origin, size: NSSize { width, height: bounds.size.height } };
                let cursor: *mut AnyObject = msg_send![class!(NSCursor), openHandCursor];
                let _: () = msg_send![this, addCursorRect: rect, cursor: cursor];
            }
        }

        unsafe {
            builder.add_method(sel!(resetCursorRects), reset_cursor_rects as extern "C" fn(_, _));
        }

        let cls = builder.register();
        CLS = Some(cls);
    });

    unsafe { CLS.unwrap() }
}

// 自定义快捷键录制文本控件：点击后成为第一响应者，捕获下一次按键作为组合键。
fn ensure_hotkey_recorder_class() -> &'static AnyClass {
    use objc2::declare::ClassBuilder;
    use std::ffi::CString;

    static mut CLS: Option<&'static AnyClass> = None;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let name = CString::new("AlacrittyHotkeyRecorderField").unwrap();
        let mut builder = ClassBuilder::new(name.as_c_str(), class!(NSTextField))
            .expect("create recorder class");

        extern "C" fn accepts_first_responder(_this: &AnyObject, _sel: Sel) -> Bool { Bool::YES }

        extern "C" fn mouse_down(this: &AnyObject, _sel: Sel, _event: *mut AnyObject) {
            unsafe {
                let win: *mut AnyObject = msg_send![this, window];
                if !win.is_null() {
                    let _: Bool = msg_send![win, makeFirstResponder: this];
                }
                let tip = NSString::from_str("录制中… 按下组合键");
                let _: () = msg_send![this, setStringValue: &*tip];
            }
        }

        extern "C" fn key_down(this: &AnyObject, _sel: Sel, event: *mut AnyObject) {
            unsafe {
                if event.is_null() { return; }
                // 取修饰与 keyCode
                let ns_flags: u64 = msg_send![event, modifierFlags];
                let carbon_mods = crate::macos::hotkey::nsflags_to_carbon_modifiers(ns_flags);
                let key_code_u: u16 = msg_send![event, keyCode];
                let key_code = key_code_u as i64;
                // ESC 视为禁用
                if key_code_u == 53 {
                    let _: () = msg_send![this, setTag: -1i64];
                    let s = NSString::from_str("禁用");
                    let _: () = msg_send![this, setStringValue: &*s];
                    let target: *mut AnyObject = msg_send![this, target];
                    let action: Sel = msg_send![this, action];
                    if !target.is_null() { let _: Bool = msg_send![this, sendAction: action, to: target]; }
                    let win: *mut AnyObject = msg_send![this, window];
                    if !win.is_null() { let _: Bool = msg_send![win, makeFirstResponder: std::ptr::null::<AnyObject>()]; }
                    return;
                }
                // 忽略纯修饰键
                let is_mod_key = matches!(key_code_u, 54 | 55 | 56 | 58 | 59 | 60 | 61 | 62 | 57);
                if is_mod_key { return; }

                // 构造展示字符串：⌘⇧⌥⌃ + 字符
                let chars_obj: *mut AnyObject = msg_send![event, charactersIgnoringModifiers];
                let mut key_text = String::new();
                if !chars_obj.is_null() {
                    let c_ptr: *const std::ffi::c_char = msg_send![chars_obj, UTF8String];
                    if !c_ptr.is_null() {
                        key_text = std::ffi::CStr::from_ptr(c_ptr).to_string_lossy().into_owned();
                    }
                }
                if key_text.is_empty() { key_text = format!("keycode:{}", key_code); }
                let mut disp = String::new();
                // NS flags bits used already; derive display from them
                const NS_MOD_SHIFT: u64 = 1 << 17;
                const NS_MOD_CTRL: u64 = 1 << 18;
                const NS_MOD_ALT: u64 = 1 << 19;
                const NS_MOD_CMD: u64 = 1 << 20;
                if ns_flags & NS_MOD_CMD != 0 { disp.push('⌘'); }
                if ns_flags & NS_MOD_SHIFT != 0 { disp.push('⇧'); }
                if ns_flags & NS_MOD_ALT != 0 { disp.push('⌥'); }
                if ns_flags & NS_MOD_CTRL != 0 { disp.push('⌃'); }
                // Uppercase letter for visibility
                disp.push_str(&key_text.to_uppercase());

                // 写入控件的 tag（高32位=mods，低32位=key_code）并更新文本
                let combined: i64 = ((carbon_mods as i64) << 32) | ((key_code as i64) & 0xFFFF_FFFF);
                let _: () = msg_send![this, setTag: combined];
                let ns_disp = NSString::from_str(&disp);
                let _: () = msg_send![this, setStringValue: &*ns_disp];

                // 回调 target/action
                let target: *mut AnyObject = msg_send![this, target];
                let action: Sel = msg_send![this, action];
                if !target.is_null() {
                    let _: Bool = msg_send![this, sendAction: action, to: target];
                }

                // 结束录制
                let win: *mut AnyObject = msg_send![this, window];
                if !win.is_null() { let _: Bool = msg_send![win, makeFirstResponder: std::ptr::null::<AnyObject>()]; }
            }
        }

        unsafe {
            builder.add_method(sel!(acceptsFirstResponder), accepts_first_responder as extern "C" fn(_, _) -> Bool);
            builder.add_method(sel!(mouseDown:), mouse_down as extern "C" fn(_, _, _));
            builder.add_method(sel!(keyDown:), key_down as extern "C" fn(_, _, _));
        }

        let cls = builder.register();
        CLS = Some(cls);
    });

    unsafe { CLS.unwrap() }
}


// 自定义 Theme 专用 NSTableView：在键盘上下移动时触发 action
fn ensure_theme_tableview_class() -> &'static AnyClass {
    use objc2::declare::ClassBuilder;
    use std::ffi::CString;

    static mut CLS: Option<&'static AnyClass> = None;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let name = CString::new("AlacrittyThemeTableView").unwrap();
        let mut builder = ClassBuilder::new(name.as_c_str(), class!(NSTableView))
            .expect("create theme table view subclass");

        extern "C" fn key_down(this: &AnyObject, _sel: Sel, event: *mut AnyObject) {
            unsafe {
                // 先让表格处理按键（更新选中行）
                let _: () = msg_send![super(this, class!(NSTableView)), keyDown: event];
                // 仅在上下方向键时触发 action，移动时也应用主题
                if !event.is_null() {
                    let key_code_u: u16 = msg_send![event, keyCode];
                    if key_code_u == 125 || key_code_u == 126 { // down/up arrows
                        let target: *mut AnyObject = msg_send![this, target];
                        let action: Sel = msg_send![this, action];
                        if !target.is_null() {
                            let _: Bool = msg_send![this, sendAction: action, to: target];
                        }
                    }
                }
            }
        }

        // 点击后确保表格成为第一响应者，方向键可用
        extern "C" fn mouse_down(this: &AnyObject, _sel: Sel, event: *mut AnyObject) {
            unsafe {
                let _: () = msg_send![super(this, class!(NSTableView)), mouseDown: event];
                let win: *mut AnyObject = msg_send![this, window];
                if !win.is_null() {
                    let _: Bool = msg_send![win, makeFirstResponder: this];
                }
            }
        }

        extern "C" fn accepts_first_responder(_this: &AnyObject, _sel: Sel) -> Bool { Bool::YES }
        extern "C" fn become_first_responder(_this: &AnyObject, _sel: Sel) -> Bool { Bool::YES }

        extern "C" fn reset_cursor_rects(this: &AnyObject, _sel: Sel) {
            unsafe {
                // 使用默认箭头光标覆盖整个表格区域
                let bounds: NSRect = msg_send![this, bounds];
                let cursor: *mut AnyObject = msg_send![class!(NSCursor), arrowCursor];
                let _: () = msg_send![this, addCursorRect: bounds, cursor: cursor];
            }
        }

        unsafe {
            builder.add_method(sel!(keyDown:), key_down as extern "C" fn(_, _, _));
            builder.add_method(sel!(mouseDown:), mouse_down as extern "C" fn(_, _, _));
            builder.add_method(sel!(acceptsFirstResponder), accepts_first_responder as extern "C" fn(_, _) -> Bool);
            builder.add_method(sel!(becomeFirstResponder), become_first_responder as extern "C" fn(_, _) -> Bool);
            builder.add_method(sel!(resetCursorRects), reset_cursor_rects as extern "C" fn(_, _));
        }

        let cls = builder.register();
        CLS = Some(cls);
    });

    unsafe { CLS.unwrap() }
}

// Theme 列表单元格：左侧文本，右侧“✓”对齐
fn ensure_theme_cellview_class() -> &'static AnyClass {
    use objc2::declare::ClassBuilder;
    use std::ffi::CString;

    static mut CLS: Option<&'static AnyClass> = None;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let name = CString::new("AlacrittyThemeCellView").unwrap();
        let mut builder = ClassBuilder::new(name.as_c_str(), class!(NSTableCellView))
            .expect("create theme cell view subclass");

        extern "C" fn layout(this: &AnyObject, _sel: Sel) {
            unsafe {
                let bounds: NSRect = msg_send![this, bounds];
                let h = bounds.size.height;
                let w = bounds.size.width;
                let left_pad: f64 = 12.0;
                let right_pad: f64 = 12.0;
                let text_h: f64 = 18.0;
                let check_w: f64 = 16.0;
                let pad_y = ((h - text_h).max(0.0)) / 2.0;
                let flipped: Bool = msg_send![this, isFlipped];
                let is_flipped = flipped == Bool::YES;
                let text_y = if is_flipped { pad_y } else { h - text_h - pad_y };

                let check: *mut AnyObject = msg_send![this, viewWithTag: 2102isize];
                let text: *mut AnyObject = msg_send![this, viewWithTag: 2101isize];

                // 右侧勾：靠右对齐
                if !check.is_null() {
                    let _: () = msg_send![check, setFrame: NSRect {
                        origin: NSPoint { x: (w - right_pad - check_w).max(0.0), y: text_y },
                        size: NSSize { width: check_w, height: text_h },
                    }];
                }

                // 左侧文本：占据余下空间
                if !text.is_null() {
                    let right_limit = if check.is_null() { w - right_pad } else { (w - right_pad - check_w - 6.0).max(left_pad) };
                    let text_w = (right_limit - left_pad).max(30.0);
                    let _: () = msg_send![text, setFrame: NSRect {
                        origin: NSPoint { x: left_pad, y: text_y },
                        size: NSSize { width: text_w, height: text_h },
                    }];
                }
            }
        }

        unsafe {
            builder.add_method(sel!(layout), layout as extern "C" fn(_, _));
        }

        let cls = builder.register();
        CLS = Some(cls);
    });

    unsafe { CLS.unwrap() }
}

// 自定义 NSTableCellView：在 layout 阶段将文本视图垂直居中并设置左右内边距
fn ensure_path_cellview_class() -> &'static AnyClass {
    use objc2::declare::ClassBuilder;
    use std::ffi::CString;

    static mut CLS: Option<&'static AnyClass> = None;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let name = CString::new("AlacrittyPathCellView").unwrap();
        let mut builder = ClassBuilder::new(name.as_c_str(), class!(NSTableCellView))
            .expect("create table cell view subclass");

        extern "C" fn layout(this: &AnyObject, _sel: Sel) {
            unsafe {
                let bounds: NSRect = msg_send![this, bounds];
                let h = bounds.size.height;
                let w = bounds.size.width;
                let left_pad: f64 = 8.0;
                let right_pad: f64 = 8.0;
                let text_h: f64 = 18.0;
                let pad_y = ((h - text_h).max(0.0)) / 2.0;
                let flipped: Bool = msg_send![this, isFlipped];
                let is_flipped = flipped == Bool::YES;
                let text_y = if is_flipped { pad_y } else { h - text_h - pad_y };
                let text_w = (w - left_pad - right_pad).max(30.0);

                let text: *mut AnyObject = msg_send![this, viewWithTag: 1002isize];
                if !text.is_null() {
                    let _: () = msg_send![text, setFrame: NSRect { origin: NSPoint { x: left_pad, y: text_y }, size: NSSize { width: text_w, height: text_h } }];
                }
            }
        }

        unsafe {
            builder.add_method(sel!(layout), layout as extern "C" fn(_, _));
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

    let mut used_icon = false;
    unsafe { used_icon = set_status_item_icon(&item); }
    if used_icon {
        // 对图标项使用方形宽度
        unsafe { let _: () = msg_send![&*item, setLength: -2.0f64]; }
    }
    if !used_icon {
        let title = NSString::from_str(text);
        item.setTitle(Some(&title));
    }

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

        // 动态插入：已保存的目录（在列表顶部）
        let saved = get_saved_paths_string();
        let mut added_any = false;
        for line in saved.lines() {
            let p = line.trim();
            if p.is_empty() { continue; }
            // 允许在配置中用 "---" 作为分隔线
            if p == "---" {
                let sep_item: *mut AnyObject = msg_send![class!(NSMenuItem), separatorItem];
                let _: () = msg_send![menu, addItem: sep_item];
                // 分隔线不计入“是否添加了可点击项”
                continue;
            }
            // 以 text: 开头的行为“不可点击文本项”
            if let Some(rest) = p.strip_prefix("text:") {
                let text = rest.trim();
                let title = NSString::from_str(text);
                let empty_key = NSString::from_str("");
                let mi_alloc: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
                let mi: *mut AnyObject = msg_send![
                    mi_alloc,
                    initWithTitle: &*title,
                    action: sel!(onStatusItemOpenSavedPath:),
                    keyEquivalent: &*empty_key
                ];
                // 不可点击
                let _: () = msg_send![mi, setEnabled: false];
                let _: () = msg_send![menu, addItem: mi];
                added_any = true;
                continue;
            }
            // 菜单标题展示 `~`，但 representedObject 保留绝对路径
            // 过长路径在中间使用省略号，避免菜单过宽
            let display = crate::path_util::shorten_home_and_ellipsize(p, 50);
            let title = NSString::from_str(&display);
            let empty_key = NSString::from_str("");
            let mi_alloc: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
            let mi: *mut AnyObject = msg_send![
                mi_alloc,
                initWithTitle: &*title,
                action: sel!(onStatusItemOpenSavedPath:),
                keyEquivalent: &*empty_key
            ];
            // 把原始路径放入 representedObject，供回调取用
            let rep = NSString::from_str(p);
            let _: () = msg_send![mi, setRepresentedObject: &*rep];
            let _: () = msg_send![mi, setTarget: target];
            let _: () = msg_send![menu, addItem: mi];
            added_any = true;
        }

        // 顶部列表与常规项之间加一条分隔线（如有目录）
        if added_any {
            let sep: *mut AnyObject = msg_send![class!(NSMenuItem), separatorItem];
            let _: () = msg_send![menu, addItem: sep];
        }

        // 新建窗口菜单项
        let title = NSString::from_str("新建窗口");
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

        // 配置菜单项
        let cfg_title = NSString::from_str("配置");
        let mi2_alloc: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
        let mi2: *mut AnyObject = msg_send![
            mi2_alloc,
            initWithTitle: &*cfg_title,
            action: sel!(onStatusItemOpenConfig:),
            keyEquivalent: &*empty_key
        ];
        let _: () = msg_send![mi2, setTarget: target];
        let _: () = msg_send![menu, addItem: mi2];

        // 主题窗口入口（位于“配置”后）
        let theme_title = NSString::from_str("主题");
        let mi_theme_alloc: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
        let mi_theme: *mut AnyObject = msg_send![
            mi_theme_alloc,
            initWithTitle: &*theme_title,
            action: sel!(onStatusItemOpenThemes:),
            keyEquivalent: &*empty_key
        ];
        let _: () = msg_send![mi_theme, setTarget: target];
        let _: () = msg_send![menu, addItem: mi_theme];

        // 分隔线
        let sep2: *mut AnyObject = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep2];

        // 退出菜单项
        let quit_title = NSString::from_str("退出");
        let miq_alloc: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
        let miq: *mut AnyObject = msg_send![
            miq_alloc,
            initWithTitle: &*quit_title,
            action: sel!(onStatusItemQuit:),
            keyEquivalent: &*empty_key
        ];
        let _: () = msg_send![miq, setTarget: target];
        let _: () = msg_send![menu, addItem: miq];

        menu
    }
}

/// 提供事件代理给状态栏菜单使用（用于“新建窗口”）。
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

    let mut used_icon = false;
    unsafe { used_icon = set_status_item_icon(&item); }
    if used_icon {
        unsafe { let _: () = msg_send![&*item, setLength: -2.0f64]; }
    }
    if !used_icon {
        let label = if let Some(t) = title { t.to_string() } else {
            let idx = NEXT_INDEX.fetch_add(1, Ordering::Relaxed);
            format!("窗口{idx}")
        };
        let title_ns = NSString::from_str(&label);
        item.setTitle(Some(&title_ns));
    }

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

    let mut used_icon = false;
    unsafe { used_icon = set_status_item_icon(&item); }
    if used_icon {
        unsafe { let _: () = msg_send![&*item, setLength: -2.0f64]; }
    }
    if !used_icon {
        let title_ns = NSString::from_str(title);
        item.setTitle(Some(&title_ns));
    }

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

//
// 配置窗口与路径记录逻辑
//

fn get_saved_paths_string() -> String {
    unsafe {
        let defs = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str("AlacrittyFolderPaths");
        let s_obj: *mut AnyObject = msg_send![&*defs, stringForKey: &*key];
        if s_obj.is_null() {
            return String::new();
        }
        let c_ptr: *const std::ffi::c_char = msg_send![s_obj, UTF8String];
        if c_ptr.is_null() {
            String::new()
        } else {
            let s = unsafe { std::ffi::CStr::from_ptr(c_ptr) };
            s.to_string_lossy().into_owned()
        }
    }
}

fn set_saved_paths_string(s: &str) {
    unsafe {
        let defs = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str("AlacrittyFolderPaths");
        let val = NSString::from_str(s);
        let _: () = msg_send![&*defs, setObject: &*val, forKey: &*key];
        let _: bool = msg_send![&*defs, synchronize];
    }
}

// 路径展示遵循全局工具：crate::path_util::shorten_home

/// 读取/保存全局快捷键（仅保存 keyCode，-1 表示禁用）。
pub fn get_saved_hotkey_code() -> i64 {
    unsafe {
        let defs = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str("AlacrittyGlobalHotkeyKeyCode");
        if msg_send![&*defs, respondsToSelector: sel!(integerForKey:)] {
            let v: i64 = msg_send![&*defs, integerForKey: &*key];
            return v;
        }
        -1
    }
}

pub fn set_saved_hotkey_code(code: i64) {
    unsafe {
        let defs = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str("AlacrittyGlobalHotkeyKeyCode");
        let _: () = msg_send![&*defs, setInteger: code, forKey: &*key];
        let _: bool = msg_send![&*defs, synchronize];
    }
}

/// 读取/保存全局快捷键的修饰位（Carbon 位编码）。
pub fn get_saved_hotkey_modifiers() -> i64 {
    unsafe {
        let defs = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str("AlacrittyGlobalHotkeyModifiers");
        if msg_send![&*defs, respondsToSelector: sel!(integerForKey:)] {
            let v: i64 = msg_send![&*defs, integerForKey: &*key];
            return v;
        }
        0
    }
}

pub fn set_saved_hotkey_modifiers(mods: i64) {
    unsafe {
        let defs = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str("AlacrittyGlobalHotkeyModifiers");
        let _: () = msg_send![&*defs, setInteger: mods, forKey: &*key];
        let _: bool = msg_send![&*defs, synchronize];
    }
}

pub fn get_saved_hotkey_display() -> String {
    unsafe {
        let defs = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str("AlacrittyGlobalHotkeyDisplay");
        let s_obj: *mut AnyObject = msg_send![&*defs, stringForKey: &*key];
        if s_obj.is_null() { return String::new(); }
        let c_ptr: *const std::ffi::c_char = msg_send![s_obj, UTF8String];
        if c_ptr.is_null() { String::new() } else { std::ffi::CStr::from_ptr(c_ptr).to_string_lossy().into_owned() }
    }
}

pub fn set_saved_hotkey_all(code: i64, mods: i64, display: &str) {
    set_saved_hotkey_code(code);
    set_saved_hotkey_modifiers(mods);
    unsafe {
        let defs = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str("AlacrittyGlobalHotkeyDisplay");
        let val = NSString::from_str(display);
        let _: () = msg_send![&*defs, setObject: &*val, forKey: &*key];
        let _: bool = msg_send![&*defs, synchronize];
    }
}

fn update_config_table() {
    unsafe {
        let table = CONFIG_TABLE_PTR.load(Ordering::Relaxed);
        if table.is_null() { return; }
        let _: () = msg_send![table, reloadData];
        if msg_send![table, respondsToSelector: sel!(sizeLastColumnToFit)] {
            let _: () = msg_send![table, sizeLastColumnToFit];
        }
        // 触发重置光标区域
        if msg_send![table, respondsToSelector: sel!(resetCursorRects)] {
            let _: () = msg_send![table, resetCursorRects];
        }
    }
}

fn update_theme_table() {
    unsafe {
        let table = THEME_TABLE_PTR.load(Ordering::Relaxed);
        if table.is_null() { return; }
        let _: () = msg_send![table, reloadData];
        if msg_send![table, respondsToSelector: sel!(sizeLastColumnToFit)] {
            let _: () = msg_send![table, sizeLastColumnToFit];
        }
        // 将选中行与“当前主题”对齐，避免 reload 后高亮停留在旧行
        if let Some(cur) = read_current_theme_expanded() {
            let themes = list_theme_files();
            for (i, p) in themes.iter().enumerate() {
                if expand_tilde(&theme_path_to_tilde(p)) == cur {
                    let set: Retained<AnyObject> = msg_send![class!(NSIndexSet), indexSetWithIndex: i as u64];
                    let _: () = msg_send![table, selectRowIndexes: &*set, byExtendingSelection: false];
                    let _: () = msg_send![table, scrollRowToVisible: i as isize];
                    break;
                }
            }
        }
    }
}

/// 若当前 App 的 keyWindow 就是“配置”窗口，则返回 true。
/// 用于在“应用从非激活切回激活”的边沿判断是否因点击配置窗口而触发，
/// 若是，则不应恢复显示所有终端窗口。
pub fn config_window_is_key_window() -> bool {
    unsafe {
        let win = CONFIG_WINDOW_PTR.load(Ordering::Relaxed);
        if win.is_null() { return false; }
        let app: *mut NSApplication = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() { return false; }
        let key: *mut AnyObject = msg_send![app, keyWindow];
        if !key.is_null() && key == win {
            return true;
        }
        // 备用判断：mainWindow 也可能为配置窗口
        let main: *mut AnyObject = msg_send![app, mainWindow];
        if !main.is_null() && main == win {
            return true;
        }
        let is_key: Bool = msg_send![win, isKeyWindow];
        is_key == Bool::YES
    }
}

/// 重新为所有状态栏项重建右键菜单（用于添加/更新目录后生效）。
fn rebuild_all_context_menus() {
    HANDLER_MAP.with(|map| {
        let mut map = map.borrow_mut();
        // 收集键以避免借用冲突
        let keys: Vec<*mut AnyObject> = map.keys().copied().collect();
        for handler_ptr in keys {
            if let Some(rec) = map.get_mut(&handler_ptr) {
                let new_menu = build_context_menu_for_target(handler_ptr);
                rec.menu = new_menu;
            }
        }
    });
}

/// 选择目录并追加到记录
pub unsafe fn pick_and_append_folder_path() {
    // NSOpenPanel
    let panel: *mut AnyObject = msg_send![class!(NSOpenPanel), openPanel];
    if panel.is_null() { return; }
    let _: () = msg_send![panel, setCanChooseFiles: false];
    let _: () = msg_send![panel, setCanChooseDirectories: true];
    let _: () = msg_send![panel, setAllowsMultipleSelection: false];
    let title = NSString::from_str("选择文件夹");
    let _: () = msg_send![panel, setTitle: &*title];

    let resp: i64 = msg_send![panel, runModal];
    // NSModalResponseOK == 1
    if resp != 1 { return; }

    let url: *mut AnyObject = msg_send![panel, URL];
    if url.is_null() { return; }
    let path_ns: *mut AnyObject = msg_send![url, path];
    if path_ns.is_null() { return; }
    let c_ptr: *const std::ffi::c_char = msg_send![path_ns, UTF8String];
    if c_ptr.is_null() { return; }
    let path = unsafe { std::ffi::CStr::from_ptr(c_ptr) }.to_string_lossy().into_owned();

    // 读取现有并去重追加
    let mut lines: Vec<String> = get_saved_paths_string()
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if !lines.iter().any(|s| s == &path) {
        lines.push(path);
    }
    let new_content = lines.join("\n");
    set_saved_paths_string(&new_content);
    update_config_table();
    // 列表改变后，重建所有右键菜单
    rebuild_all_context_menus();
}

/// 打开（或聚焦）配置窗口
pub unsafe fn open_config_window() {
    assert!(MainThreadMarker::new().is_some());
    let existing = CONFIG_WINDOW_PTR.load(Ordering::Relaxed);
    if !existing.is_null() {
        // 若通过配置入口激活应用，仅希望显示配置窗口本身；
        // 抑制一次“恢复全部终端窗口”。
        crate::macos::activation_guard::suppress_next_activation_restore();
        // 确保已存在的配置窗口也会移动到当前桌面
        if msg_send![existing, respondsToSelector: sel!(setCollectionBehavior:)]
            && msg_send![existing, respondsToSelector: sel!(collectionBehavior)]
        {
            let existing_flags: u64 = msg_send![existing, collectionBehavior];
            let move_to_active_space: u64 = 1u64 << 1; // MoveToActiveSpace
            let transient: u64 = 1u64 << 3;           // Transient
            let combined = existing_flags | move_to_active_space | transient;
            let _: () = msg_send![existing, setCollectionBehavior: combined];
        }
        let _: () = msg_send![existing, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
        let _: () = msg_send![existing, center];
        update_config_table();
        return;
    }

    // 创建窗口
    let w_alloc: *mut AnyObject = msg_send![class!(NSWindow), alloc];
    // 520x380 窗口
    let frame = NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: 520.0, height: 380.0 } };
    let titled: u64 = 1u64 << 0; // NSWindowStyleMaskTitled
    let closable: u64 = 1u64 << 1; // NSWindowStyleMaskClosable
    let miniaturizable: u64 = 1u64 << 2; // NSWindowStyleMaskMiniaturizable
    let resizable: u64 = 1u64 << 3; // NSWindowStyleMaskResizable
    let style_mask = titled | closable | miniaturizable | resizable;
    let backing_buffered: u64 = 2; // NSBackingStoreBuffered
    let win: *mut AnyObject = msg_send![
        w_alloc,
        initWithContentRect: frame,
        styleMask: style_mask,
        backing: backing_buffered,
        defer: false
    ];
    if win.is_null() { return; }

    // 标题
    let title = NSString::from_str("配置");
    let _: () = msg_send![win, setTitle: &*title];
    // 关闭时不释放对象，避免持有的全局指针悬挂
    let _: () = msg_send![win, setReleasedWhenClosed: false];

    // 确保“配置”窗口也在当前桌面（Space）显示
    // 通过设置 NSWindowCollectionBehaviorMoveToActiveSpace | NSWindowCollectionBehaviorTransient
    if msg_send![win, respondsToSelector: sel!(setCollectionBehavior:)]
        && msg_send![win, respondsToSelector: sel!(collectionBehavior)]
    {
        let existing: u64 = msg_send![win, collectionBehavior];
        let move_to_active_space: u64 = 1u64 << 1; // MoveToActiveSpace
        let transient: u64 = 1u64 << 3;           // Transient
        let combined = existing | move_to_active_space | transient;
        let _: () = msg_send![win, setCollectionBehavior: combined];
    }

    // 内容视图
    let content_view: *mut AnyObject = msg_send![win, contentView];
    if content_view.is_null() { return; }
    if msg_send![content_view, respondsToSelector: sel!(setAutoresizesSubviews:)] {
        let _: () = msg_send![content_view, setAutoresizesSubviews: true];
    }
    let cv_frame: NSRect = msg_send![content_view, frame];
    let pad: f64 = 16.0;
    let btn_h: f64 = 28.0;
    let btn_w: f64 = 28.0; // 使用方形小按钮呈现“＋/－”
    let hk_h: f64 = 24.0;  // 顶部“全局快捷键”行高

    // 计算布局：按钮在底部左侧（Finder 风格）
    let btn_x = 16.0f64;
    let btn_y = pad;
    let btn_frame_plus = NSRect { origin: NSPoint { x: btn_x, y: btn_y }, size: NSSize { width: btn_w, height: btn_h } };
    let btn_gap = 8.0f64;
    let btn_frame_minus = NSRect { origin: NSPoint { x: btn_x + btn_w + btn_gap, y: btn_y }, size: NSSize { width: btn_w, height: btn_h } };
    // “分隔线”按钮更宽一些，便于显示文字
    let sep_w: f64 = 64.0;
    let btn_frame_sep = NSRect { origin: NSPoint { x: btn_x + (btn_w + btn_gap) * 2.0, y: btn_y }, size: NSSize { width: sep_w, height: btn_h } };
    // “文本”按钮尺寸与分隔线类似，放在其右侧
    let txt_w: f64 = 64.0;
    let btn_frame_txt = NSRect {
        origin: NSPoint { x: btn_x + (btn_w + btn_gap) * 2.0 + sep_w + btn_gap, y: btn_y },
        size: NSSize { width: txt_w, height: btn_h },
    };

    let scroll_x = pad;
    // 底部预留按钮区
    let scroll_y = pad + btn_h + pad;
    let scroll_w = cv_frame.size.width - 2.0 * pad;
    // 额外为顶部“全局快捷键”留出 hk_h + pad
    let scroll_h = cv_frame.size.height - (3.0 * pad) - btn_h - (hk_h + pad);
    let scroll_frame = NSRect { origin: NSPoint { x: scroll_x, y: scroll_y }, size: NSSize { width: scroll_w, height: scroll_h } };

    // 按钮：＋ / －
    let cls = ensure_click_handler_class();
    let handler: Retained<AnyObject> = msg_send![cls, new];

    // ＋ 按钮（添加）
    let btn_title_plus = NSString::from_str("＋");
    let button_plus: *mut AnyObject = msg_send![class!(NSButton), alloc];
    let button_plus: *mut AnyObject = msg_send![button_plus, initWithFrame: btn_frame_plus];
    let _: () = msg_send![button_plus, setTitle: &*btn_title_plus];
    let _: () = msg_send![button_plus, setTarget: &*handler];
    let _: () = msg_send![button_plus, setAction: sel!(onConfigAddPath:)];
    // 固定在左下角：Flexible 右/上边距
    if msg_send![button_plus, respondsToSelector: sel!(setAutoresizingMask:)] {
        // NSViewMaxXMargin | NSViewMaxYMargin
        let mask: u64 = (1u64 << 2) | (1u64 << 5);
        let _: () = msg_send![button_plus, setAutoresizingMask: mask];
    }

    // － 按钮（移除选中）
    let btn_title_minus = NSString::from_str("－");
    let button_minus: *mut AnyObject = msg_send![class!(NSButton), alloc];
    let button_minus: *mut AnyObject = msg_send![button_minus, initWithFrame: btn_frame_minus];
    let _: () = msg_send![button_minus, setTitle: &*btn_title_minus];
    let _: () = msg_send![button_minus, setTarget: &*handler];
    let _: () = msg_send![button_minus, setAction: sel!(onConfigRemoveSelected:)];
    if msg_send![button_minus, respondsToSelector: sel!(setAutoresizingMask:)] {
        // NSViewMaxXMargin | NSViewMaxYMargin
        let mask: u64 = (1u64 << 2) | (1u64 << 5);
        let _: () = msg_send![button_minus, setAutoresizingMask: mask];
    }

    // “分隔线”按钮（在选中行后插入 ---）
    let btn_title_sep = NSString::from_str("分隔线");
    let button_sep: *mut AnyObject = msg_send![class!(NSButton), alloc];
    let button_sep: *mut AnyObject = msg_send![button_sep, initWithFrame: btn_frame_sep];
    let _: () = msg_send![button_sep, setTitle: &*btn_title_sep];
    let _: () = msg_send![button_sep, setTarget: &*handler];
    let _: () = msg_send![button_sep, setAction: sel!(onConfigAddSeparator:)];
    if msg_send![button_sep, respondsToSelector: sel!(setAutoresizingMask:)] {
        // NSViewMaxXMargin | NSViewMaxYMargin
        let mask: u64 = (1u64 << 2) | (1u64 << 5);
        let _: () = msg_send![button_sep, setAutoresizingMask: mask];
    }

    // “文本”按钮（在选中行后插入 text:...）
    let btn_title_txt = NSString::from_str("文本");
    let button_txt: *mut AnyObject = msg_send![class!(NSButton), alloc];
    let button_txt: *mut AnyObject = msg_send![button_txt, initWithFrame: btn_frame_txt];
    let _: () = msg_send![button_txt, setTitle: &*btn_title_txt];
    let _: () = msg_send![button_txt, setTarget: &*handler];
    let _: () = msg_send![button_txt, setAction: sel!(onConfigAddText:)];
    if msg_send![button_txt, respondsToSelector: sel!(setAutoresizingMask:)] {
        // NSViewMaxXMargin | NSViewMaxYMargin
        let mask: u64 = (1u64 << 2) | (1u64 << 5);
        let _: () = msg_send![button_txt, setAutoresizingMask: mask];
    }

    // 滚动 + 表格视图显示路径列表
    let scroll: *mut AnyObject = msg_send![class!(NSScrollView), alloc];
    let scroll: *mut AnyObject = msg_send![scroll, initWithFrame: scroll_frame];
    // 让滚动区域随窗口变化而自适应宽高
    if msg_send![scroll, respondsToSelector: sel!(setAutoresizingMask:)] {
        // NSViewWidthSizable | NSViewHeightSizable
        let mask: u64 = (1u64 << 1) | (1u64 << 4);
        let _: () = msg_send![scroll, setAutoresizingMask: mask];
    }
    // 配置窗口应使用 PathTableView（显示“小手”光标，便于表达可操作/可拖拽）
    let table_cls = ensure_path_tableview_class();
    let table: *mut AnyObject = msg_send![table_cls, alloc];
    let table: *mut AnyObject = msg_send![table, initWithFrame: NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: scroll_w, height: scroll_h } }];
    if msg_send![table, respondsToSelector: sel!(setAutoresizingMask:)] {
        // NSViewWidthSizable | NSViewHeightSizable
        let mask: u64 = (1u64 << 1) | (1u64 << 4);
        let _: () = msg_send![table, setAutoresizingMask: mask];
    }
    let col: *mut AnyObject = msg_send![class!(NSTableColumn), alloc];
    let identifier = NSString::from_str("PathColumn");
    let col: *mut AnyObject = msg_send![col, initWithIdentifier: &*identifier];
    let _: () = msg_send![col, setWidth: scroll_w];
    // 让唯一列跟随表格宽度自动调整
    if msg_send![col, respondsToSelector: sel!(setResizingMask:)] {
        // NSTableColumnAutoresizingMask = 1
        let _: () = msg_send![col, setResizingMask: 1u64];
    }
    if msg_send![table, respondsToSelector: sel!(setColumnAutoresizingStyle:)] {
        // 使用“最后一列自适应”策略更符合单列列表
        // NSTableViewLastColumnOnlyAutoresizingStyle 的值在 0..4 之间，这里取 4 以覆盖该常量
        let _: () = msg_send![table, setColumnAutoresizingStyle: 4u64];
    }
    let _: () = msg_send![table, addTableColumn: col];
    if msg_send![table, respondsToSelector: sel!(sizeLastColumnToFit)] {
        let _: () = msg_send![table, sizeLastColumnToFit];
    }
    // 隐藏表头
    let _: () = msg_send![table, setHeaderView: std::ptr::null::<AnyObject>()];
    // 行背景：交替颜色显示
    let _: () = msg_send![table, setUsesAlternatingRowBackgroundColors: true];
    if msg_send![table, respondsToSelector: sel!(setGridStyleMask:)] {
        let _: () = msg_send![table, setGridStyleMask: 0u64];
    }
    if msg_send![table, respondsToSelector: sel!(setBackgroundColor:)] {
        let bg: *mut AnyObject = msg_send![class!(NSColor), controlBackgroundColor];
        let _: () = msg_send![table, setBackgroundColor: bg];
    }
    let _: () = msg_send![table, setRowHeight: 22.0f64];
    let spacing = NSSize { width: 0.0, height: 2.0 };
    let _: () = msg_send![table, setIntercellSpacing: spacing];
    // 单选即可（便于移动顺序）
    let _: () = msg_send![table, setAllowsMultipleSelection: false];
    // dataSource / delegate 使用 handler
    let _: () = msg_send![table, setDataSource: &*handler];
    let _: () = msg_send![table, setDelegate: &*handler];
    // 注册拖拽类型并限定为本地移动
    let drag_type = NSString::from_str("com.alacritty.pathrow");
    let types: *mut AnyObject = msg_send![class!(NSArray), arrayWithObject: &*drag_type];
    let _: () = msg_send![table, registerForDraggedTypes: types];
    let op_move: u64 = 16; // NSDragOperationMove
    let _: () = msg_send![table, setDraggingSourceOperationMask: op_move, forLocal: true];
    let _: () = msg_send![table, setDraggingSourceOperationMask: op_move, forLocal: false];
    // 嵌入滚动视图
    let _: () = msg_send![scroll, setHasVerticalScroller: true];
    if msg_send![scroll, respondsToSelector: sel!(setDrawsBackground:)] {
        let _: () = msg_send![scroll, setDrawsBackground: true];
    }
    if msg_send![scroll, respondsToSelector: sel!(setBorderType:)] {
        let _: () = msg_send![scroll, setBorderType: 0u64];
    }
    let clip: *mut AnyObject = msg_send![scroll, contentView];
    if !clip.is_null() && msg_send![clip, respondsToSelector: sel!(setDrawsBackground:)] {
        let _: () = msg_send![clip, setDrawsBackground: true];
    }
    let _: () = msg_send![scroll, setDocumentView: table];

    // 顶部：全局快捷键 录制
    let label_frame = NSRect { origin: NSPoint { x: pad, y: cv_frame.size.height - pad - hk_h }, size: NSSize { width: 90.0, height: hk_h } };
    let label: *mut AnyObject = msg_send![class!(NSTextField), alloc];
    let label: *mut AnyObject = msg_send![label, initWithFrame: label_frame];
    let ltext = NSString::from_str("全局快捷键");
    let _: () = msg_send![label, setStringValue: &*ltext];
    let _: () = msg_send![label, setBezeled: false];
    let _: () = msg_send![label, setEditable: false];
    let _: () = msg_send![label, setSelectable: false];
    if msg_send![label, respondsToSelector: sel!(setDrawsBackground:)] {
        let _: () = msg_send![label, setDrawsBackground: false];
    }
    if msg_send![label, respondsToSelector: sel!(setAutoresizingMask:)] {
        // 顶部固定：底部距父视图的间距可伸缩（MinYMargin），右侧间距可伸缩（MaxXMargin），宽度不自适应
        // 这样在窗口拉伸时，始终贴顶且保持与左侧距离不变、宽度不变
        // NSViewMinYMargin = 1<<3, NSViewMaxXMargin = 1<<2
        let mask: u64 = (1u64 << 3) | (1u64 << 2);
        let _: () = msg_send![label, setAutoresizingMask: mask];
    }

    // 录制区：自定义 TextField
    let rec_x = pad + 90.0 + 8.0;
    let rec_w = 220.0;
    let rec_frame = NSRect { origin: NSPoint { x: rec_x, y: cv_frame.size.height - pad - hk_h + 1.0 }, size: NSSize { width: rec_w, height: hk_h } };
    let rec_cls = ensure_hotkey_recorder_class();
    let recorder: *mut AnyObject = msg_send![rec_cls, alloc];
    let recorder: *mut AnyObject = msg_send![recorder, initWithFrame: rec_frame];
    // 外观
    let _: () = msg_send![recorder, setBezeled: true];
    let _: () = msg_send![recorder, setEditable: false];
    let _: () = msg_send![recorder, setSelectable: false];
    if msg_send![recorder, respondsToSelector: sel!(setAutoresizingMask:)] {
        // 同上：顶部固定且贴左，宽度不自适应
        // NSViewMinYMargin | NSViewMaxXMargin
        let mask: u64 = (1u64 << 3) | (1u64 << 2);
        let _: () = msg_send![recorder, setAutoresizingMask: mask];
    }
    // 目标与响应：录制完成后回调 handler
    let _: () = msg_send![recorder, setTarget: &*handler];
    let _: () = msg_send![recorder, setAction: sel!(onConfigHotkeyRecorded:)];
    // 初始显示
    let saved_disp = get_saved_hotkey_display();
    let init_text = if saved_disp.is_empty() { "点击并按下组合键".to_string() } else { saved_disp };
    let init_ns = NSString::from_str(&init_text);
    let _: () = msg_send![recorder, setStringValue: &*init_ns];

    // 添加子视图
    let _: () = msg_send![content_view, addSubview: scroll];
    let _: () = msg_send![content_view, addSubview: label];
    let _: () = msg_send![content_view, addSubview: recorder];
    let _: () = msg_send![content_view, addSubview: button_plus];
    let _: () = msg_send![content_view, addSubview: button_minus];
    let _: () = msg_send![content_view, addSubview: button_sep];
    let _: () = msg_send![content_view, addSubview: button_txt];

    // 保存全局指针并设置初始内容
    CONFIG_WINDOW_PTR.store(win, Ordering::Relaxed);
    CONFIG_TABLE_PTR.store(table, Ordering::Relaxed);
    update_config_table();

    // 显示窗口：先标记抑制一次“激活后恢复全部窗口”，再激活应用。
    crate::macos::activation_guard::suppress_next_activation_restore();
    let app: *mut NSApplication = msg_send![class!(NSApplication), sharedApplication];
    let _: () = msg_send![app, activateIgnoringOtherApps: true];
    let _: () = msg_send![win, center];
    let _: () = msg_send![win, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];

    // 防止 handler 释放
    std::mem::forget(handler);
}

/// 打开（或聚焦）主题窗口
pub unsafe fn open_theme_window() {
    assert!(MainThreadMarker::new().is_some());
    let existing = THEME_WINDOW_PTR.load(Ordering::Relaxed);
    if !existing.is_null() {
        crate::macos::activation_guard::suppress_next_activation_restore();
        if msg_send![existing, respondsToSelector: sel!(setCollectionBehavior:)]
            && msg_send![existing, respondsToSelector: sel!(collectionBehavior)]
        {
            let flags: u64 = msg_send![existing, collectionBehavior];
            let move_to_active_space: u64 = 1u64 << 1; // MoveToActiveSpace
            let transient: u64 = 1u64 << 3;           // Transient
            let _: () = msg_send![existing, setCollectionBehavior: flags | move_to_active_space | transient];
        }
        let _: () = msg_send![existing, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
        let _: () = msg_send![existing, center];
        update_theme_table();
        return;
    }

    // 创建窗口
    let w_alloc: *mut AnyObject = msg_send![class!(NSWindow), alloc];
    let frame = NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: 420.0, height: 380.0 } };
    let titled: u64 = 1u64 << 0; // Titled
    let closable: u64 = 1u64 << 1; // Closable
    let miniaturizable: u64 = 1u64 << 2; // Miniaturizable
    let resizable: u64 = 1u64 << 3; // Resizable
    let style_mask = titled | closable | miniaturizable | resizable;
    let backing_buffered: u64 = 2; // Buffered
    let win: *mut AnyObject = msg_send![
        w_alloc,
        initWithContentRect: frame,
        styleMask: style_mask,
        backing: backing_buffered,
        defer: false
    ];
    if win.is_null() { return; }
    let title = NSString::from_str("主题");
    let _: () = msg_send![win, setTitle: &*title];
    let _: () = msg_send![win, setReleasedWhenClosed: false];

    if msg_send![win, respondsToSelector: sel!(setCollectionBehavior:)]
        && msg_send![win, respondsToSelector: sel!(collectionBehavior)]
    {
        let existing: u64 = msg_send![win, collectionBehavior];
        let move_to_active_space: u64 = 1u64 << 1;
        let transient: u64 = 1u64 << 3;
        let _: () = msg_send![win, setCollectionBehavior: existing | move_to_active_space | transient];
    }

    // 内容视图和表格
    let content_view: *mut AnyObject = msg_send![win, contentView];
    if content_view.is_null() { return; }
    let pad: f64 = 16.0;
    let cv_frame: NSRect = msg_send![content_view, frame];
    let scroll_frame = NSRect {
        origin: NSPoint { x: pad, y: pad },
        size: NSSize { width: cv_frame.size.width - 2.0 * pad, height: cv_frame.size.height - 2.0 * pad },
    };

    let scroll: *mut AnyObject = msg_send![class!(NSScrollView), alloc];
    let scroll: *mut AnyObject = msg_send![scroll, initWithFrame: scroll_frame];
    if msg_send![scroll, respondsToSelector: sel!(setAutoresizingMask:)] {
        // Width + Height sizable
        let mask: u64 = (1u64 << 1) | (1u64 << 4);
        let _: () = msg_send![scroll, setAutoresizingMask: mask];
    }

    let cls = ensure_click_handler_class();
    let handler: Retained<AnyObject> = msg_send![cls, new];

    // 主题窗口应使用 ThemeTableView（键盘上下移动时也触发 action，且使用箭头光标）
    let table_cls = ensure_theme_tableview_class();
    let table: *mut AnyObject = msg_send![table_cls, alloc];
    let table: *mut AnyObject = msg_send![table, initWithFrame: NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: scroll_frame.size.width, height: scroll_frame.size.height } }];
    // 提前记录全局指针，确保数据源/委托方法能识别“主题表”
    THEME_TABLE_PTR.store(table, Ordering::Relaxed);
    if msg_send![table, respondsToSelector: sel!(setAutoresizingMask:)] {
        let mask: u64 = (1u64 << 1) | (1u64 << 4);
        let _: () = msg_send![table, setAutoresizingMask: mask];
    }
    // 仅单选，不允许空选；使用常规高亮样式
    let _: () = msg_send![table, setAllowsMultipleSelection: false];
    if msg_send![table, respondsToSelector: sel!(setAllowsEmptySelection:)] {
        let _: () = msg_send![table, setAllowsEmptySelection: false];
    }
    if msg_send![table, respondsToSelector: sel!(setSelectionHighlightStyle:)] {
        // NSTableViewSelectionHighlightStyleRegular
        let _: () = msg_send![table, setSelectionHighlightStyle: 0u64];
    }
    let col: *mut AnyObject = msg_send![class!(NSTableColumn), alloc];
    let identifier = NSString::from_str("ThemeColumn");
    let col: *mut AnyObject = msg_send![col, initWithIdentifier: &*identifier];
    let _: () = msg_send![col, setWidth: scroll_frame.size.width];
    if msg_send![col, respondsToSelector: sel!(setResizingMask:)] {
        let _: () = msg_send![col, setResizingMask: 1u64];
    }
    if msg_send![table, respondsToSelector: sel!(setColumnAutoresizingStyle:)] {
        let _: () = msg_send![table, setColumnAutoresizingStyle: 4u64];
    }
    let _: () = msg_send![table, addTableColumn: col];
    if msg_send![table, respondsToSelector: sel!(sizeLastColumnToFit)] {
        let _: () = msg_send![table, sizeLastColumnToFit];
    }
    let _: () = msg_send![table, setHeaderView: std::ptr::null::<AnyObject>()];
    let _: () = msg_send![table, setUsesAlternatingRowBackgroundColors: true];
    if msg_send![table, respondsToSelector: sel!(setGridStyleMask:)] {
        let _: () = msg_send![table, setGridStyleMask: 0u64];
    }
    let _: () = msg_send![table, setRowHeight: 22.0f64];
    let spacing = NSSize { width: 0.0, height: 2.0 };
    let _: () = msg_send![table, setIntercellSpacing: spacing];
    let _: () = msg_send![table, setAllowsMultipleSelection: false];
    let _: () = msg_send![table, setDataSource: &*handler];
    let _: () = msg_send![table, setDelegate: &*handler];
    // 单击行回调：切换主题
    let _: () = msg_send![table, setTarget: &*handler];
    let _: () = msg_send![table, setAction: sel!(onThemeRowClick:)];
    // 监听选中变化通知，确保键盘/鼠标变更都立即应用主题
    let nc: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
    let name = NSString::from_str("NSTableViewSelectionDidChangeNotification");
    let _: () = msg_send![nc, addObserver: &*handler, selector: sel!(onThemeSelectionChanged:), name: &*name, object: table];

    let _: () = msg_send![scroll, setHasVerticalScroller: true];
    let _: () = msg_send![scroll, setDocumentView: table];
    let _: () = msg_send![content_view, addSubview: scroll];

    THEME_WINDOW_PTR.store(win, Ordering::Relaxed);
    update_theme_table();

    // 初始选中当前主题所在行
    if let Some(cur) = read_current_theme_expanded() {
        let mut match_idx: isize = -1;
        let themes = list_theme_files();
        for (i, p) in themes.iter().enumerate() {
            if expand_tilde(&theme_path_to_tilde(p)) == cur {
                match_idx = i as isize;
                break;
            }
        }
        if match_idx >= 0 {
            // selectRowIndexes:byExtendingSelection:
            let set: Retained<AnyObject> = msg_send![class!(NSIndexSet), indexSetWithIndex: match_idx as u64];
            let _: () = msg_send![table, selectRowIndexes: &*set, byExtendingSelection: false];
            let _: () = msg_send![table, scrollRowToVisible: match_idx];
        }
    }

    crate::macos::activation_guard::suppress_next_activation_restore();
    let app: *mut NSApplication = msg_send![class!(NSApplication), sharedApplication];
    let _: () = msg_send![app, activateIgnoringOtherApps: true];
    let _: () = msg_send![win, center];
    // 让主题表成为第一响应者，保证上下键立即生效
    let _: Bool = msg_send![win, makeFirstResponder: table];
    let _: () = msg_send![win, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];

    std::mem::forget(handler);
}
