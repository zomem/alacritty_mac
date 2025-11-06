use std::sync::atomic::{AtomicBool, Ordering};

// 用于在“激活应用”边沿时抑制一次“恢复并显示全部窗口”。
// 典型场景：仅想在后台打开/聚焦“配置”窗口，而不唤起所有终端窗口。
static SUPPRESS_ONCE: AtomicBool = AtomicBool::new(false);

/// 标记：在下一次检测到从非激活 -> 激活的边沿时，跳过恢复全部窗口。
pub fn suppress_next_activation_restore() {
    SUPPRESS_ONCE.store(true, Ordering::Relaxed);
}

/// 消费一次抑制标记；若返回 true，调用方应跳过本次恢复显示逻辑。
pub fn take_suppression() -> bool {
    SUPPRESS_ONCE.swap(false, Ordering::Relaxed)
}

