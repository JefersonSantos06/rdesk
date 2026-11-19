//! Small helpers for showing computers and sessions. (Whether a computer is online or in
//! use comes from the server; hosts tell nothing about themselves before login.)

/// "just now", "5 min ago", "3 h ago", "2 days ago" (Unix seconds).
pub fn ago(then: i64, now: i64) -> String {
    let s = (now - then).max(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        _ => {
            let d = s / 86_400;
            if d == 1 { "1 day ago".into() } else { format!("{d} days ago") }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ago_texts() {
        assert_eq!(ago(100, 130), "just now");
        assert_eq!(ago(0, 7200), "2 h ago");
        assert_eq!(ago(0, 86_400), "1 day ago");
        assert_eq!(ago(0, 86_400 * 3), "3 days ago");
    }
}
