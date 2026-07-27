# Attachments: files, images, audio, video

*How the transcript renders, downloads, and opens attachment payloads — the file card and its QuickLook preview, the image viewer and its size mirror, the native audio engine, and the video tile/player. Governs `app/ios/App/Screens/ImageViewer.swift`, `app/ios/App/Screens/VideoPlayerScreen.swift`, `app/ios/App/Core/AudioPlayerCenter.swift`, the `FilePreviewSheet` / blob-cache plumbing in `app/ios/App/Core/`, and the `AttachmentImage` / `AttachmentAudio` / `AttachmentVideo` cards in `app/ios/web/src/`.*

## File attachments

### The card and its preview

**File attachments** (`kind != "image"`) render as a tappable card whose glyph is a download arrow until the blob is on disk, then a document. Tapping fetches it; tapping a fetched one opens `FilePreviewSheet` — `QLPreviewController` wrapped in `UIViewControllerRepresentable` (SwiftUI's `quickLookPreview` is **macOS-only**; it does not exist in the iOS SDK), falling back to `UIActivityViewController` when `QLPreviewController.canPreview` says no (archives, unknown binaries).

QuickLook picks its previewer from the file **extension**, and the core's cache names files by **digest**, so `previewFile` writes `<tmp>/baybo-preview/<digest>/<real name>` first.

### Download progress

The spinning ring is **indeterminate on purpose** — the byte counter beside it (`884 KB / 2.3 MB`) is the progress.

Bytes come from the core's `BlobProgress` callback (`blob_download_bytes(blob_id, progress)`), rate-limited to one tick per 100ms **in Rust**: a 100 MiB download hands the chunk loop thousands of buffers, and every tick would otherwise cross the FFI and the webview bridge.

`downloaded` is bytes ON DISK, so a resumed download (both legs send `Range: bytes=N-`) opens at its floor instead of snapping back to zero. `blob_is_cached` is the mount-time probe.

### The blob cache

Downloaded blobs live in `Application Support/baybo/blobs` (`ClientConfig.blobCacheDir`, set in `app/ios/App/Core/Baybo.swift`) — **not** the OS temp dir, which iOS reclaims under storage pressure: a file the user downloaded stays downloaded.

The directory is excluded from backup (a blob runs to 100 MiB and is always re-fetchable) and **nothing evicts from it** — it only grows. That is deliberate; when it needs bounding it wants a stated retention policy, not a surprise sweep.

`ready` is still re-asked on every mount rather than remembered, because the directory is a fact about disk.

### Per-blob state subscription

Cards subscribe to `fileState` **by blob id** (`onFileState`), so a progress tick re-renders one card and `MessageRow`'s memo survives — and two cards on the same blob (an agent's file the user quotes back) update together.

### Long-press to share

**A long-press on any DOWNLOADED file / audio / video card shares it** (`useSharePress` → `shareFile` → `ChatStore.fileShare` → the system sheet on the materialised file, real name intact).

The synthetic click that follows the lift is swallowed in the capture phase so a share never also downloads/plays/previews; an undownloaded card ignores the hold and keeps its plain tap. The audio card's seek bar stops `touchstart` propagation so a slow scrub can't arm the share.

Images share from inside their viewer; the video player carries the same top-right share button.

### Buffered bridge answers

**Bridge ANSWERS buffer across the detach window like frames do**: a `fileState` (or `videoPoster` reply) that lands while no webview is attached is stashed in the store (`pendingFileStates` last-write-wins per blob / `pendingPosterReplies`) and flushed on `attachBridge` — a download whose terminal `ready` fell while the user was parked on the list used to wedge its card at `loading` forever, because a SAME-session re-attach remounts nothing and so re-queries nothing.

A flushed poster reply whose session switched away settles nothing web-side (`init` cleared `posterPending`) and is ignored.

## Images and the image viewer

An inline `image` attachment does NOT use the file-card path above — a decoded `AttachmentImage` is a button whose tap posts `viewImage` (blob id only), and `ChatStore.viewImage` decodes the device-cached blob into a `UIImage` and presents `ImageViewer` (`app/ios/App/Screens/ImageViewer.swift`) via `.fullScreenCover`.

That is a dedicated `UIScrollView`-backed zoomable viewer (pinch, double-tap-to-fit/restore, single-tap or ✕ to close, image fades onto a black field) rather than QuickLook: QuickLook embedded in a SwiftUI `.sheet` gave no reliable double-tap-to-restore (the sheet's gestures fight it), and the black edge-to-edge field matches chat images where the document previewer's white chrome does not. The blob is already on disk from the thumbnail fetch (`requestBlob` → `blob_download_bytes` writes the cache), so it opens instantly. (Files still use `previewFile` → QuickLook, above.)

### Zoom fit comes from `layoutSubviews`, never `updateUIView`

**Compute the zoom fit from the scroll view's `layoutSubviews`, never from `updateUIView`** — SwiftUI calls `updateUIView` BEFORE UIKit lays the scroll view out, so `bounds` is still zero, a `bounds > 0` guard bails, and it never runs again: min = max = zoomScale = 1, the image renders at native size, pinch does nothing, and double-tap has no smaller scale to restore to (this exact bug shipped once).

Re-fit only when `bounds.size` actually changes, or the layout passes that zooming itself triggers re-seat the image frame and fight the zoom.

### Sharing from the viewer

The viewer's top-right button opens the system share sheet on the blob materialised under its real name (`writePreviewFile`, shared with the file path) — the FILE, not the decoded `UIImage`, so Save-to-Photos / Files / AirDrop keep the original encoding.

That share sheet is why the app carries **`NSPhotoLibraryAddUsageDescription`**: without it iOS TERMINATES the app the moment the user taps "Save Image".

`ContentBlock::Image`/`Audio` carry an `Option<String>` filename end to end (`AttachFile` → transcript → `split_content` → `WireAttachment.filename`), so an agent's image shares under its REAL name; a genuinely nameless one (pasted screenshot, MCP bitmap) falls back to `attachment.<ext>` derived from the mime. Transcripts persisted before that field existed still load (`#[serde(default)]`) — they simply have no name, so an OLD message's image keeps sharing as `attachment.png`.

### Reserved size: `imageDims` + `.attachment-bubble.sized`

**An image the transcript has decoded before shows NO loading state at all.**

Every decode records the image's natural `[w,h]` (keyed by the blob's sha256 DIGEST — the read token rotates, the digest doesn't) into `PersistedState.imageDims`, so it rides the per-session mirror to disk. On the next open the bubble is `sized`: `.attachment-bubble.sized` (`app/ios/web/src/styles.css`) solves the same contain-fit the `<img>` will (`min(100%, natural, --attachment-max-h × ratio)` + `aspect-ratio`) and reserves the EXACT final box from the first paint — no 12rem tile, no spinner, no release.

That release was the bug: a re-opened thread grew/shrank every image row as its bytes landed (measured: page height 3332 → 3396 px on ONE image), and WKWebView has no scroll anchoring to absorb it, so the page shook under the reader.

**The fit MUST be solved on the BUBBLE**: the frame's containing block is the bubble, a shrink-to-fit flex item, where a `%` width is cyclic and WebKit resolves it to zero (a 0×0 reservation).

The mirror can outlive its blobs (a restored backup carries the transcript, not the blob cache), so the spinner still exists inside the reserved box — just delayed 400ms in CSS, invisible on the cache hit it exists to skip. An image with no recorded size (first view, a scrolled-up history page) keeps the old tile and records its size on the way through.

## Audio attachments

**Audio attachments** (`kind == "audio"`) render as the file card with the glyph slot promoted to a play/pause control once the blob is on disk (`AttachmentAudio`; the download flow is the file card's, unchanged).

### Duration rides the wire

The track's LENGTH rides the wire — `WireAttachment.duration_ms`, probed by `AttachFile` at attach time (the one moment the file is in hand server-side) and carried through `ContentBlock::Audio` → `split_content` → the REST `ChatAttachment` — so the resting card reads `MP3 · 3:23 · 3.3 MB` before any byte is downloaded or played; `None` (inbound channel audio, old rows) just drops the middle segment.

The probe must not trust headers or extensions (measured on synthetic 240s files):

- a VBR MP3 without a Xing header estimates **6× off** from first-frame bitrate math, so `audio/mpeg` gets a full frame walk (`mp3-duration`);
- an Opus stream inside `.ogg` fails lofty's extension guess, so lofty runs behind a content sniff (`guess_file_type`).

### The engine is native

The ENGINE is native — `AudioPlayerCenter` (`app/ios/App/Core/AudioPlayerCenter.swift`), ONE `AVPlayer` app-wide — driven over the bridge (`audioToggle`/`audioSeek`/`queryAudioState` in, `audioState` pushes out: play/pause flips, 2 Hz position ticks, `stopped` on end/usurp).

Native rather than an in-webview `<audio>` because the bytes never cross the bridge as base64, AVAudioSession `.playback` means the ringer switch can't silence it, and — with `UIBackgroundModes: audio` (`app/ios/project.yml`) + Now Playing + remote commands — a track keeps playing through lock/background with Control Center transport while the user stays IN the chat.

Backing out to the chat LIST stops it (`AppStore.chatPath`'s didSet, when the last `.session` route leaves the stack — **NOT** `ChatScreen.onDisappear`, which also fires under fullScreenCovers like the image viewer): audio with no visible card to control it reads as a bug.

Playback runs off the materialised preview file (`materializePreviewFile` — AVPlayer sniffs the container by extension). Starting a track stops the previous one and tells its card `stopped`; a card mounting mid-playback resyncs via `queryAudioState`; `resetChatStores` (logout/rebind) stops the player outright.

### Engine-truth invariants

Each covers a wedge that review found:

- the card mirrors EVERY engine flip via **KVO on `timeControlStatus`** — the system pauses without any interruption notice (headphones unplugged, a stall) and the card would otherwise wedge on "playing" with an inverted toggle;
- `AVPlayerItem.status == .failed` / `failedToPlayToEndTime` reset the card to rest (an unplayable blob must not play dead air forever);
- "is it playing" checks are `timeControlStatus != .paused` (right after `play()` the engine sits in `.waiting…` — intent is playing);
- an `ended` latch keeps a finished track answering `stopped` to late `queryAudioState` (the player stays loaded for instant replay, but a remounting card must not resync to an engaged "paused @ 0:00" the live card never showed).

### Precise duration

The engine opens the asset with `AVURLAssetPreferPreciseDurationAndTiming` — the default duration is a bitrate GUESS on headerless/VBR containers (a 4:00 ogg reported 5:04), and the seek bar maps fractions onto it, so an imprecise duration also mis-aims every scrub. The card remembers the precise engine duration and never falls back to the wire estimate it disproved.

### The seek bar

The seek bar (`AudioTrack`) renders in EVERY state so the card's height never jumps as playback starts/ends — inert and empty until the engine engages (a tap on it bubbles to the card and just plays).

Engaged: drags scrub locally, commit ONE `audioSeek` on lift, and the committed value keeps rendering until the engine's next push (native answers a seek with an optimistic state) — dropping it at lift would snap the fill back to the pre-seek playhead. `touch-action: none` so a scrub never scrolls the thread.

## Video attachments

**Video attachments** (`kind == "file"` + `video/*` mime — video has no wire kind of its own; `isVideoAttachment` elects the tile by mime) render as a fixed-width tile in the image idiom (`AttachmentVideo`).

### Tile states

- **Undownloaded**: a blank surface with a centered download disc and `1:23 · 24 MB` in a corner chip — the LENGTH rides the wire like audio's (`ContentBlock::File` carries `duration_ms` for videos; `AttachFile` probes mp4/mov via the `mp4` crate and webm/mkv via `matroska`), the size is what a tap commits to, and once the bytes are local the chip drops to just the length.
- **While fetching**: the disc becomes a DETERMINATE progress ring (the attachment declares its total; the corner chip counts bytes).
- **Downloaded**: native supplies a poster frame + duration over `requestVideoPoster` (`AVAssetImageGenerator` on the materialised file, first frame downscaled to ≤1024px JPEG — cached as `poster.jpg` + `poster.json` beside the preview file, because the tile re-requests on EVERY remount and the generator is too heavy to re-run each time) and the disc becomes a play glyph.

### The player

Tapping a downloaded tile posts `playVideo` → `ChatStore.videoPlayback` presents `VideoPlayerScreen` (`app/ios/App/Screens/VideoPlayerScreen.swift`) via `.fullScreenCover`: an embedded `AVPlayerViewController` on a black field with the viewer chrome's ✕ disc (`ViewerChromeButton`, shared with `ImageViewer`) — embedded AVKit shows no Done button, only owned presentations get one.

Chat audio is stopped before the video presents (two engines over one AVAudioSession fight), and `playVideo` bails if the bridge detached while the file materialised — the user backed out, and presenting late would arm a stale `fullScreenCover` for the NEXT entry.

Poster/play materialisations coalesce in-flight per target path (`previewMaterializations`) so a poster request racing a play tap doesn't hold the video in memory twice.

### Reserved ratio

The poster's natural size is recorded into the same `ImageDimsStore`/`imageDims` mirror keyed by blob digest, so a re-opened thread reserves the tile's ratio from the first paint; the ratio is clamped to [3:4, 16:9] (`clampVideoRatio`) and the cover-fit poster absorbs the clamp as a crop.
