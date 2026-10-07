use thiserror::Error;

/// Fatal framing failures on the browser link. A well-framed message whose
/// JSON does not parse is not an error here: the gateway codec yields it as
/// `LinkMessage::Malformed` so the link survives it.
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("link io: {0}")]
    Io(#[from] std::io::Error),

    #[error("link message announces {len} bytes, max is {max}")]
    MessageTooLarge { len: usize, max: usize },

    #[error("link message is empty (no kind byte)")]
    EmptyMessage,

    #[error("unknown link message kind {kind}")]
    UnknownKind { kind: u8 },

    #[error("link json message is {len} bytes, max is {max}")]
    JsonTooLarge { len: usize, max: usize },

    #[error("frame message is shorter than its header length prefix")]
    FrameTruncated,

    #[error("frame header is {len} bytes, max is {max}")]
    FrameHeaderTooLarge { len: usize, max: usize },

    #[error("frame header length {header_len} exceeds the {body_len}-byte frame body")]
    FrameHeaderOverrun { header_len: usize, body_len: usize },

    #[error("frame jpeg is {len} bytes, max is {max}")]
    FrameTooLarge { len: usize, max: usize },

    #[error("frame carries an empty jpeg")]
    EmptyJpeg,

    #[error("unexpected frame message on a json-only link direction")]
    UnexpectedFrame,

    #[error("link json decode: {reason}")]
    Decode { reason: String },

    #[error("link json encode: {reason}")]
    Encode { reason: String },
}

/// Failures setting up the gateway's end of the browser link.
#[derive(Debug, Error)]
pub enum BrowserViewError {
    #[error("no browser link socket path fits the {max}-byte sun_path limit")]
    SocketPathTooLong { max: usize },

    #[error("browser link socket dir {path}: {reason}")]
    SocketDir { path: String, reason: String },

    #[error("bind browser link socket {path}: {reason}")]
    Bind { path: String, reason: String },
}

/// Why a web viewer could not subscribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ViewError {
    #[error("too many browser viewers (max {max})")]
    TooManyViewers { max: usize },
}
