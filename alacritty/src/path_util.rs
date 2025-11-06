/// 路径展示辅助：将用户主目录前缀替换为 `~`。
/// 仅用于 UI 显示，不改变实际路径值。
pub fn shorten_home(p: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if p == home {
            return "~".to_string();
        }
        if p.starts_with(&home) {
            let suffix = &p[home.len()..];
            if suffix.is_empty() || suffix.starts_with('/') {
                return format!("~{}", suffix);
            }
        }
    }
    p.to_string()
}

