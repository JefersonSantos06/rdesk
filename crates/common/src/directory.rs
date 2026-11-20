//! Small helpers for showing computers and sessions. (Whether a computer is online or in
//! use comes from the server; hosts tell nothing about themselves before login.)

/// "just now", "5 min ago", "3 h ago", "2 days ago" (Unix seconds).
pub fn ago(then: i64, now: i64) -> String {
    let s = (now - then).max(0);
    let (m, h, d) = (s / 60, s / 3600, s / 86_400);
    match s {
        0..60 => crate::t!("just now", "agora há pouco").into(),
        60..3600 => crate::tf!("{m} min ago", "há {m} min"),
        3600..86_400 => crate::tf!("{h} h ago", "há {h} h"),
        _ if d == 1 => crate::t!("1 day ago", "há 1 dia").into(),
        _ => crate::tf!("{d} days ago", "há {d} dias"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ago_texts() {
        let _lang = crate::i18n::TEST_LOCK.lock().unwrap();
        assert_eq!(ago(100, 130), "just now");
        assert_eq!(ago(0, 7200), "2 h ago");
        assert_eq!(ago(0, 86_400), "1 day ago");
        assert_eq!(ago(0, 86_400 * 3), "3 days ago");
    }
}
