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

/// 文本过长时在中间使用省略号进行截断。
/// `max_chars` 为最大显示字符数（按 `char` 计数）。
pub fn ellipsize_middle(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let keep = max_chars - 3; // 除去 "..." 的长度
    let left = keep / 2;
    let right = keep - left;

    let left_part: String = s.chars().take(left).collect();
    let right_rev: String = s.chars().rev().take(right).collect();
    let right_part: String = right_rev.chars().rev().collect();

    format!("{}...{}", left_part, right_part)
}

/// 结合主目录缩写与中间省略：用于 UI 友好展示路径。
pub fn shorten_home_and_ellipsize(p: &str, max_chars: usize) -> String {
    let sh = shorten_home(p);
    ellipsize_middle(&sh, max_chars)
}
