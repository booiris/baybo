//! The gateway's own enforcement of the `LinkLimits` it announced in
//! `HelloAck`. The sidecar clamps before sending; this is the second check,
//! so a sidecar that ignores the limits still cannot hand viewers more than
//! they were promised. Lengths are UTF-16 code units, as the limits define.

use crate::limits::{
    MAX_LINK_TARGETS, MAX_LINK_TEXT_CHARS, MAX_TARGET_ID_CHARS, MAX_TARGET_URL_CHARS,
};
use crate::wire::{BrowserPhase, BrowserStatus, StreamStatus, TargetId, TargetInfo, TargetsMsg};

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Truncate `text` to at most `max` UTF-16 units, on a char boundary.
fn clamp_utf16(text: &mut String, max: u32) {
    let max = max as usize;
    let mut units = 0;
    for (at, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units > max {
            text.truncate(at);
            return;
        }
    }
}

fn id_fits(id: &TargetId) -> bool {
    utf16_len(id.as_str()) <= MAX_TARGET_ID_CHARS as usize
}

/// `None` when the target's id is over the limit (the sidecar drops such a
/// target rather than truncating it); otherwise url and title clamped.
fn clamp_target(mut target: TargetInfo) -> Option<TargetInfo> {
    if !id_fits(&target.target_id) {
        return None;
    }
    clamp_utf16(&mut target.url, MAX_TARGET_URL_CHARS);
    clamp_utf16(&mut target.title, MAX_LINK_TEXT_CHARS);
    Some(target)
}

pub(crate) fn targets(msg: TargetsMsg) -> TargetsMsg {
    TargetsMsg {
        browser_gen: msg.browser_gen,
        targets: msg
            .targets
            .into_iter()
            .filter_map(clamp_target)
            .take(MAX_LINK_TARGETS as usize)
            .collect(),
        followed: msg.followed.filter(id_fits),
    }
}

pub(crate) fn stream(msg: StreamStatus) -> StreamStatus {
    StreamStatus {
        target: msg.target.and_then(clamp_target),
        ..msg
    }
}

pub(crate) fn status(mut msg: BrowserStatus) -> BrowserStatus {
    if let BrowserPhase::Failed { error } = &mut msg.phase {
        clamp_utf16(error, MAX_LINK_TEXT_CHARS);
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{BrowserMode, StreamState};

    fn target(id: &str, url: &str, title: &str) -> TargetInfo {
        TargetInfo {
            target_id: TargetId::new(id),
            url: url.into(),
            title: title.into(),
        }
    }

    #[test]
    fn clamp_counts_utf16_units_and_never_splits_a_char() {
        let mut s = format!("{}{}", "a".repeat(255), '\u{1F600}');
        clamp_utf16(&mut s, 256);
        assert_eq!(s, "a".repeat(255));
        let mut s = format!("a{}", '\u{1F600}');
        clamp_utf16(&mut s, 3);
        assert_eq!(utf16_len(&s), 3);
        let mut short = String::from("abc");
        clamp_utf16(&mut short, 256);
        assert_eq!(short, "abc");
    }

    #[test]
    fn targets_are_capped_filtered_and_clamped() {
        let long_id = "T".repeat(MAX_TARGET_ID_CHARS as usize + 1);
        let mut list: Vec<_> = (0..MAX_LINK_TARGETS + 5)
            .map(|i| target(&format!("T{i}"), &"u".repeat(1000), &"t".repeat(1000)))
            .collect();
        list.insert(0, target(&long_id, "https://a/", ""));
        let msg = targets(TargetsMsg {
            browser_gen: 3,
            targets: list,
            followed: Some(TargetId::new(long_id.clone())),
        });
        assert_eq!(msg.targets.len(), MAX_LINK_TARGETS as usize);
        assert_eq!(msg.targets[0].target_id.as_str(), "T0");
        assert!(msg.targets.iter().all(|t| {
            t.url.len() == MAX_TARGET_URL_CHARS as usize
                && t.title.len() == MAX_LINK_TEXT_CHARS as usize
        }));
        assert_eq!(msg.followed, None);
    }

    #[test]
    fn stream_target_and_failed_error_are_clamped() {
        let msg = stream(StreamStatus {
            state: StreamState::Live,
            browser_gen: 1,
            target: Some(target("T1", &"u".repeat(1000), "x")),
        });
        assert_eq!(
            msg.target.map(|t| t.url.len()),
            Some(MAX_TARGET_URL_CHARS as usize)
        );
        let msg = status(BrowserStatus {
            mode: BrowserMode::Host,
            phase: BrowserPhase::Failed {
                error: "e".repeat(1000),
            },
            browser_gen: 1,
        });
        assert_eq!(
            msg.phase,
            BrowserPhase::Failed {
                error: "e".repeat(MAX_LINK_TEXT_CHARS as usize)
            }
        );
    }
}
