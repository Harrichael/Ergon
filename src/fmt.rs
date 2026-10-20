use chrono::{Local, TimeZone};

/// `2026-10-20 14:03 (3h ago)`, in local time.
pub fn timestamp(ts: i64, now: i64) -> String {
    let local = match Local.timestamp_opt(ts, 0).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => ts.to_string(),
    };
    format!("{local} ({})", age(now - ts))
}

fn age(secs: i64) -> String {
    match secs {
        ..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(age(-5), "just now");
        assert_eq!(age(59), "just now");
        assert_eq!(age(60), "1m ago");
        assert_eq!(age(3 * 3600 + 59), "3h ago");
        assert_eq!(age(86400 * 2), "2d ago");
    }
}
