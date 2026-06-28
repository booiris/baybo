export interface BaseInfo {
  channel_version?: string;
}

export const MessageType = {
  NONE: 0,
  USER: 1,
  BOT: 2,
} as const;

export const MessageItemType = {
  NONE: 0,
  TEXT: 1,
  IMAGE: 2,
  VOICE: 3,
  FILE: 4,
  VIDEO: 5,
} as const;

export const MessageState = {
  NEW: 0,
  GENERATING: 1,
  FINISH: 2,
} as const;

export const TypingStatus = {
  TYPING: 1,
  CANCEL: 2,
} as const;

export interface TextItem {
  text?: string;
}

export interface CDNMedia {
  encrypt_query_param?: string;
  /** Base64-encoded AES key. Two encodings observed on the wire: raw
   * 16 bytes (image media) and base64-of-32-hex-char-string (file /
   * voice / video). See `cdn/pic-decrypt.ts::parseAesKey`. */
  aes_key?: string;
  /** 0 = encrypt fileid only, 1 = bundle thumb / mid info. */
  encrypt_type?: number;
  /** Server-supplied download URL. Preferred over client-side
   * construction; the iLink CDN is the source of truth on routing. */
  full_url?: string;
}

export interface ImageItem {
  media?: CDNMedia;
  thumb_media?: CDNMedia;
  /** Raw AES-128 key as 32 hex chars. When present, takes precedence
   * over `media.aes_key` for inbound decryption (newer protocol). */
  aeskey?: string;
  url?: string;
  mid_size?: number;
  thumb_size?: number;
  thumb_height?: number;
  thumb_width?: number;
  hd_size?: number;
}

export interface VoiceItem {
  media?: CDNMedia;
  /** 1=pcm, 2=adpcm, 3=feature, 4=speex, 5=amr, 6=silk, 7=mp3, 8=ogg-speex. */
  encode_type?: number;
  bits_per_sample?: number;
  /** Sample rate in Hz. */
  sample_rate?: number;
  /** Playback length in milliseconds. */
  playtime?: number;
  /** Server-side STT result. When present, treat the message as text
   * and skip the media download — the user already saw the text on
   * their end and the agent doesn't need the raw audio. */
  text?: string;
}

export interface FileItem {
  media?: CDNMedia;
  file_name?: string;
  md5?: string;
  /** Plaintext size as a string (the iLink wire encoding). */
  len?: string;
}

export interface VideoItem {
  media?: CDNMedia;
  video_size?: number;
  play_length?: number;
  video_md5?: string;
  thumb_media?: CDNMedia;
  thumb_size?: number;
  thumb_height?: number;
  thumb_width?: number;
}

export interface RefMessage {
  message_item?: MessageItem;
  /** Quoted-message summary line ("摘要"). */
  title?: string;
}

/** proto: UploadMediaType — selects the right CDN preflight. */
export const UploadMediaType = {
  IMAGE: 1,
  VIDEO: 2,
  FILE: 3,
  VOICE: 4,
} as const;

export interface GetUploadUrlReq {
  filekey?: string;
  /** See `UploadMediaType`. */
  media_type?: number;
  to_user_id?: string;
  /** Plaintext size of the original file. */
  rawsize?: number;
  /** MD5 of the plaintext. */
  rawfilemd5?: string;
  /** Ciphertext size after AES-128-ECB padding. */
  filesize?: number;
  /** Plaintext size of the thumbnail (IMAGE/VIDEO only). */
  thumb_rawsize?: number;
  /** MD5 of the thumbnail plaintext. */
  thumb_rawfilemd5?: string;
  /** Ciphertext size of the thumbnail. */
  thumb_filesize?: number;
  /** Set true to skip thumbnail upload preflight. */
  no_need_thumb?: boolean;
  /** AES-128 encryption key. */
  aeskey?: string;
}

export interface GetUploadUrlResp {
  /** Upload param for the main file. */
  upload_param?: string;
  /** Upload param for the thumbnail. Empty when `no_need_thumb=true`. */
  thumb_upload_param?: string;
  /** Server-built upload URL. Preferred over client-side construction. */
  upload_full_url?: string;
}

export interface MessageItem {
  type?: number;
  create_time_ms?: number;
  update_time_ms?: number;
  is_completed?: boolean;
  msg_id?: string;
  ref_msg?: RefMessage;
  text_item?: TextItem;
  image_item?: ImageItem;
  voice_item?: VoiceItem;
  file_item?: FileItem;
  video_item?: VideoItem;
}

export interface WeixinMessage {
  seq?: number;
  message_id?: number;
  from_user_id?: string;
  to_user_id?: string;
  client_id?: string;
  create_time_ms?: number;
  update_time_ms?: number;
  delete_time_ms?: number;
  session_id?: string;
  group_id?: string;
  message_type?: number;
  message_state?: number;
  item_list?: MessageItem[];
  context_token?: string;
}

export interface GetUpdatesReq {
  get_updates_buf?: string;
}

export interface GetUpdatesResp {
  ret?: number;
  errcode?: number;
  errmsg?: string;
  msgs?: WeixinMessage[];
  get_updates_buf?: string;
  longpolling_timeout_ms?: number;
}

export interface SendMessageReq {
  msg?: WeixinMessage;
}

export interface SendTypingReq {
  ilink_user_id?: string;
  typing_ticket?: string;
  status?: number;
}

export interface SendTypingResp {
  ret?: number;
  errmsg?: string;
}

export interface GetConfigResp {
  ret?: number;
  errmsg?: string;
  typing_ticket?: string;
}
