use std::time::Duration;

/// Version carried in `Hello` / `HelloAck`; a mismatch is rejected.
pub const BROWSER_LINK_PROTOCOL_VERSION: u32 = 1;

/// Largest JPEG payload a single screencast frame may carry.
pub(crate) const MAX_SCREENCAST_FRAME_BYTES: usize = 2 * 1024 * 1024;

/// Largest serialized `FrameHeader` JSON inside a frame message.
pub(crate) const MAX_FRAME_HEADER_BYTES: usize = 4096;

/// Largest JSON (`LinkUp` / `LinkDown`) message body on the link. Sized so
/// the largest clamped `TargetsMsg` fits even when every clamped string is
/// worst-case escaped (see `wire::tests::largest_clamped_targets_msg_fits`).
pub(crate) const MAX_LINK_JSON_BYTES: usize = 256 * 1024;

/// Most tabs the sidecar lists in one `TargetsMsg`; extra tabs are dropped.
pub(crate) const MAX_LINK_TARGETS: u32 = 32;

/// Longest CDP target id the sidecar forwards; a target with a longer id is
/// dropped rather than truncated. Lengths below are UTF-16 code units (JS
/// `string.length`), so the sidecar clamps with a plain `slice`.
pub(crate) const MAX_TARGET_ID_CHARS: u32 = 64;

/// Longest `TargetInfo.url` the sidecar forwards; longer URLs are truncated.
pub(crate) const MAX_TARGET_URL_CHARS: u32 = 512;

/// Longest free text (`TargetInfo.title`, `BrowserPhase::Failed.error`) the
/// sidecar forwards; longer text is truncated.
pub(crate) const MAX_LINK_TEXT_CHARS: u32 = 256;

/// Width of the big-endian length prefixes used by the link framing.
pub(crate) const LEN_PREFIX_BYTES: usize = 4;

/// Width of the message kind byte.
pub(crate) const KIND_BYTES: usize = 1;

/// Largest value the link-level `total_len` prefix may announce: the kind
/// byte plus the biggest possible body (a frame: header length prefix,
/// header, JPEG).
pub(crate) const MAX_LINK_MESSAGE_BYTES: usize =
    KIND_BYTES + LEN_PREFIX_BYTES + MAX_FRAME_HEADER_BYTES + MAX_SCREENCAST_FRAME_BYTES;

const _: () = assert!(MAX_LINK_MESSAGE_BYTES >= KIND_BYTES + MAX_LINK_JSON_BYTES);
const _: () = assert!(MAX_LINK_MESSAGE_BYTES <= u32::MAX as usize);

/// [`MAX_SCREENCAST_FRAME_BYTES`] as announced to the sidecar in `HelloAck`.
pub(crate) const MAX_SCREENCAST_FRAME_BYTES_U32: u32 = MAX_SCREENCAST_FRAME_BYTES as u32;

/// [`MAX_LINK_JSON_BYTES`] as announced to the sidecar in `HelloAck`.
pub(crate) const MAX_LINK_JSON_BYTES_U32: u32 = MAX_LINK_JSON_BYTES as u32;

/// Largest text message a web viewer may send on the browser-view WS.
pub const MAX_BROWSER_VIEW_CLIENT_MSG_BYTES: usize = 16 * 1024;

/// Concurrent web viewers allowed per gateway.
pub const MAX_VIEWERS: usize = 8;

/// How long a freshly accepted link has to deliver its `Hello`.
pub(crate) const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest a single WS send to a viewer may block before the viewer is
/// dropped.
pub const WS_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the web viewer sends `Ping`; announced in `ViewerDown::Link`.
pub(crate) const VIEWER_PING_INTERVAL_MS: u32 = 15_000;

/// A viewer that sends no `Ping` for this long is closed.
pub const VIEWER_PING_TIMEOUT: Duration = Duration::from_millis(2 * VIEWER_PING_INTERVAL_MS as u64);

#[cfg(test)]
mod tests {
    //! The sidecar and its tests copy a few protocol constants and the
    //! `HelloAck` limits by hand (ts-rs exports types, not values). These
    //! tests read those files and fail when a copy drifts from this crate.

    use std::path::{Path, PathBuf};

    use super::*;
    use crate::codec::{KIND_FRAME, KIND_JSON};
    use crate::wire::{LinkDown, LinkLimits};

    fn sidecar_file(rel: &str) -> (PathBuf, String) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../sidecars/tool/browser")
            .join(rel);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        (path, text)
    }

    /// The integer written right after the first `marker` in `text`
    /// (`_` digit separators allowed).
    fn number_after(path: &Path, text: &str, marker: &str) -> u64 {
        let at = text
            .find(marker)
            .unwrap_or_else(|| panic!("{} has no `{marker}`", path.display()))
            + marker.len();
        let digits: String = text[at..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '_')
            .filter(|c| *c != '_')
            .collect();
        digits
            .parse()
            .unwrap_or_else(|e| panic!("{}: `{marker}` is not a number: {e}", path.display()))
    }

    #[test]
    fn sidecar_mirrors_match() {
        let (path, text) = sidecar_file("src/view_link.ts");
        let consts: [(&str, u64); 6] = [
            (
                "BROWSER_LINK_PROTOCOL_VERSION",
                u64::from(BROWSER_LINK_PROTOCOL_VERSION),
            ),
            ("KIND_JSON", u64::from(KIND_JSON)),
            ("KIND_FRAME", u64::from(KIND_FRAME)),
            ("LEN_PREFIX_BYTES", LEN_PREFIX_BYTES as u64),
            ("KIND_BYTES", KIND_BYTES as u64),
            ("MAX_FRAME_HEADER_BYTES", MAX_FRAME_HEADER_BYTES as u64),
        ];
        for (name, expected) in consts {
            let marker = format!("export const {name} =");
            assert_eq!(
                number_after(&path, &text, &marker),
                expected,
                "{name} in {}",
                path.display()
            );
        }
    }

    #[test]
    fn sidecar_test_hello_ack_limits_match() {
        let LinkDown::HelloAck { protocol, limits } = LinkDown::hello_ack() else {
            panic!("hello_ack is a HelloAck");
        };
        let LinkLimits {
            max_frame_bytes,
            max_json_bytes,
            max_targets,
            max_target_id_chars,
            max_target_url_chars,
            max_text_chars,
        } = limits;
        let fields = [
            ("max_frame_bytes:", max_frame_bytes),
            ("max_json_bytes:", max_json_bytes),
            ("max_targets:", max_targets),
            ("max_target_id_chars:", max_target_id_chars),
            ("max_target_url_chars:", max_target_url_chars),
            ("max_text_chars:", max_text_chars),
        ];
        for rel in [
            "test/e2e_screencast.test.mjs",
            "test/screencast.test.mjs",
            "test/view_link.test.mjs",
        ] {
            let (path, text) = sidecar_file(rel);
            for (marker, expected) in fields {
                assert_eq!(
                    number_after(&path, &text, marker),
                    u64::from(expected),
                    "{marker} in {}",
                    path.display()
                );
            }
        }
        for rel in ["test/e2e_screencast.test.mjs", "test/view_link.test.mjs"] {
            let (path, text) = sidecar_file(rel);
            assert_eq!(
                number_after(&path, &text, "protocol:"),
                u64::from(protocol),
                "protocol in {}",
                path.display()
            );
        }
    }
}
