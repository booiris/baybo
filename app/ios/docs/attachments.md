# Attachments: files, images, audio, video

*How the transcript renders, downloads, and opens attachment payloads — the file card and its QuickLook preview, the image viewer and its size mirror, the native audio engine, and the video tile/player — plus how the composer stages an OUTBOUND one. Governs `app/ios/App/Screens/ImageViewer.swift`, `app/ios/App/Screens/VideoPlayerScreen.swift`, `app/ios/App/Core/AudioPlayerCenter.swift`, the `FilePreviewSheet` / blob-cache plumbing in `app/ios/App/Core/`, the staging half of `app/ios/App/Screens/ComposerView.swift`, `app/ios/App/Support/Pasteboard.swift`, the paste hook in `app/ios/App/AppDelegate.swift`, and the `AttachmentImage` / `AttachmentAudio` / `AttachmentVideo` cards in `app/ios/web/src/`.*

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

### Every card is viewport-gated, not just the image

`useNearViewport` (`Transcript.tsx`) is the load-once IntersectionObserver gate the image tile always had, lifted to serve **every** attachment card: `useFileState` and `useAudioState` take it, so `queryFileState` / `queryAudioState` — and therefore `requestVideoPoster`, which waits on `ready` — fire only once the card is within the preload band.

A restored thread mounts every card it holds at once, and each ungated one is a bridge round trip on the app's MAIN thread, landing squarely in the window the transcript is trying to paint its first frame in: a post out and an `evaluateJavaScript` back apiece, plus an `AVAssetImageGenerator` per downloaded video. A long conversation carries dozens; the reader can see two or three. Nothing is lost by waiting — a card can only be downloaded or played by being tapped, which needs it on screen.

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

An inline `image` attachment does NOT use the file-card path above — a decoded `AttachmentImage` is a button whose tap posts `viewImage` (blob id only), and `ChatStore.viewImage` turns the device-cached blob into a `ViewedImage.Content` and presents `ImageViewer` (`app/ios/App/Screens/ImageViewer.swift`) via `.fullScreenCover`.

That is a dedicated `UIScrollView`-backed zoomable viewer (pinch, double-tap-to-fit/restore, single-tap or ✕ to close, image fades onto a black field) rather than QuickLook: QuickLook embedded in a SwiftUI `.sheet` gave no reliable double-tap-to-restore (the sheet's gestures fight it), and the black edge-to-edge field matches chat images where the document previewer's white chrome does not. The blob is already on disk from the thumbnail fetch (`requestBlob` → `blob_download_bytes` writes the cache), so it opens instantly. (Files still use `previewFile` → QuickLook, above.)

### A vector is a second medium, not a second format

**`UIImage(data:)` is nil for an SVG on every iOS there is** — no public API decodes one (`CGImageSourceCreateWithData` returns nil and ImageIO does not carry the type; verified on iOS 26). For as long as the viewer asked only `UIImage`, a tap on an agent-drawn diagram fell out of `viewImage`'s `guard` and did *nothing whatsoever*: no viewer, no error, no log.

So `ViewedImage.Content` has two cases and the election is `UIImage` first, mime second (`.raster(UIImage)` / `.vector(Data)`). A vector renders in `ZoomableVectorView`, a `WKWebView` — because ZOOM is the whole reason a chat image goes full screen, and WebKit re-renders vector art at every scale, where a rasterised copy would have to pick a resolution at open time and go soft past it. The chrome (✕, share, the fade onto black) is the same SwiftUI layer over both.

Two things about that web view are load-bearing:

- the page is a **data-URI `<img>`, never the SVG as the document**. An SVG document runs its own `<script>`; an SVG inside an `<img>` cannot, by spec — and these bytes are agent-authored. A `default-src 'none'` CSP and `loadHTMLString(baseURL: nil)` (a unique opaque origin) close the rest, and the configuration carries no message handlers, so it shares nothing with the transcript's bridge.
- **both taps are bound by hand**, exactly as the raster viewer binds them — double to zoom toward the point (3× from fit) and back, single to close, waiting on the double to fail so a zoom is never read as a dismiss. Leaving the zoom to WebKit does not work: its double tap is *smart magnification*, "zoom to the block under the finger", and this page is one image already fitted to the viewport — so it computes that there is nothing to do and the gesture does nothing at all (measured: the art stayed 123pt either way). Ours drives the web view's own `scrollView`, the same one its pinch moves, so the two never disagree about the current scale. `AttachmentImageUITests.testDoubleTapZoomsAVectorAndBack` measures it off the SCREEN's pixels — the page scale lives inside the web view and nothing above the seam can read it.

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

#### A vector's size cannot be read off the element showing it

**WebKit answers `naturalWidth` for an SVG with the size it is laid out at RIGHT NOW.** The same 1200×400 page measures 1200 on a detached image, 192 while it decodes inside the 12rem loading tile, and 358 once released into the reading column (all three measured). Recording what the element reported — which is what every image did — meant a wide diagram rendered full width on its first paint and came back **a third of the column** on every open after it, reserved at the size of the tile it happened to decode in.

So a vector (`isVectorImage`, `image/svg+xml`) is measured BEFORE it paints: `measureIntrinsicSize` points a **detached `Image`** — one never inserted into the document, so no layout can colour the answer — at the same object URL, and hands the result up to the bubble (`onIntrinsicSize`) before the real `<img>` is given its `src`. The raster path is untouched: a PNG's pixel count is a property of its bytes, and the extra decode would buy nothing.

Two things follow from measuring first rather than on load:

- **an SVG written as a bare `viewBox` appears at all.** It has no intrinsic width, and the bubble is a shrink-to-fit flex item, so with no box reserved WebKit lays it out at ZERO — invisible, and untappable with it.
- **a stale entry corrects itself.** The mirror is a file and outlives the fix, so a thread opened today can still carry the number its loading tile handed over; the measurement overwrites it on sight instead of sizing that bubble wrong for the life of the thread. This is the one exception to the read-once-at-mount rule above, and it is safe for the same reason the rule exists: nothing has painted under it yet.

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

Backing out to the chat LIST stops it (`AppStore.chatPath`'s didSet, when the last `.session` route leaves the stack — **NOT** `ChatScreen.onDisappear`, which also fires under fullScreenCovers like the image viewer): audio with no visible card to control it reads as a bug. The composer's draft is checkpointed off that same didSet, for the mirror-image reason — see [The draft belongs to the SESSION](#the-draft-belongs-to-the-session-not-to-the-composer-frame).

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

## Outbound staging (the composer)

*The other direction: how a pick becomes a `WireAttachment`. Everything above is what the transcript does with one that has already landed.*

### Three sources behind one `+`

The composer's `+` opens `AttachMenuPanel` (hand-rolled, overlaid on the dock with its scrim over the transcript — see [navigation.md](navigation.md) § The composer pill) with **Photos** (`PhotosPicker`, `matching: .images`), **Files** (`.fileImporter`, no type restriction) and — only when the clipboard holds an image — **Paste**. Photos and Files are MULTI-select and a paste stages every image item on the board; the strip holds at most `ChatStore.maxStagedAttachments` (10) and the over-cap picks raise the composer notice.

**The panel owns no staging state.** It is presented by `ChatScreen` while both pickers stay modifiers on `ComposerView` — their selection cap reads the strip's free slots and their results feed `ComposerStaging` directly — so a row tap travels down as an `AttachSource` request (`AttachMenu.pick`) and the picker answering it clears the request as it dismisses.

**The row set is snapshotted as the panel goes UP**, by `AttachMenu.toggle(pasteReady:)`, and the panel is handed that array (`sources`) rather than reading `AttachSource.allCases`. Both halves of that are load-bearing. `panelHeight` feeds the offset that positions the panel *before* it lays out, so a height derived from `allCases` would reserve three rows for a two-row panel and float it a whole row too high; and re-deciding the predicate inside `ChatScreen.body` would add or drop a row mid-`fade`, since the `+` republishes its anchor on every tick of the focus and keyboard animations.

### Paste is a third source, and it has no picker

The Paste row is the one row with nothing out of process behind it, and that changes two things. (It is also not the only way in — see § Long-press → Paste below — but it is the one that works for every clipboard shape.)

**Nothing dismisses it, so `pickerBinding` cannot serve it.** That binding retires `AttachMenu.pick` on a *picker's dismissal* (`guard !presented`); a row that leaves `pick` set works exactly ONCE, because the second tap re-assigns the same value and `@Published` publishes no change. `ComposerView` clears `attach.pick` itself, in the same turn it calls `ComposerStaging.stagePasteboard()`. `ComposerAttachUITests.testPasteRowStagesAnImageEveryTime` pastes twice for exactly this reason — it is the only end-to-end staging witness the UI tier has, since both pickers are unreachable from XCUITest.

**Presence and bytes are two different reads, and only one of them is free.** `UIPasteboard`'s `types(forItemSet:)` / `hasImages` / `numberOfItems` are documented as NOT notifying the user, so `ComposerStaging.pasteReady` (which decides whether the row appears at all) costs nothing. Pulling the bytes is the opposite: for content copied in another app it raises the system "Allow Paste?" alert, and the read **blocks the thread it is on** until the user answers — measured, ~90s of a wedged main thread in a spike. So `loadPasted` pulls off the main actor via `Task.detached`, and the presence probe is never used as a stand-in for the pull (or vice versa). `PasteboardReading` is the seam: `SystemPasteboard` in production, `FakePasteboard` in tests (the real board is process-global and swift-testing runs suites in parallel — the same contamination `TempSupportDir` exists to prevent), and `DemoPasteboard` behind `-baybo-demo-paste`.

**Tiles are admitted from the item indices, before any bytes exist** — the same invariant a photo batch has (§ Send gating, and the retry), reached here by counting the items that *declare* an image rather than by loading them. That drive is shared, not restated: `admitThenLoad` takes every slot up front and then fills them ONE AT A TIME, because a load materialises a whole encoded pick (ten at once is ten full-size `Data`s alive together) and because the `work` handle has to land on the tile before the load's body can run, or a ✕ tapped mid-batch has nothing to cancel. A photo batch and a paste differ only in which loader they hand it.

The security-scope bracket below is a **Files-only** concern: a pasted image has no URL to scope.

### Long-press → Paste catches the image-only case, and only that

There are TWO ways in, and the second one is deliberately partial.

Long-pressing the field offers Paste whatever the clipboard holds — iOS puts that item there, and it is there even for an **empty** board — but a SwiftUI `TextField` only inserts text, so on an image it did nothing at all. That is not a missing menu item; it is a missing handler, and it cannot be diagnosed by looking at the menu.

The handler lives on the **responder chain**. UIKit asks the first responder for `canPerformAction(paste:)` and, only when it says no, walks up looking for someone who can. `AppDelegate` is the chain's terminus — which is why it is a `UIResponder` and not an `NSObject`; as an `NSObject` it is not in the chain at all, and SwiftUI owns every view between the field and the window, so there is no other ancestor to hang this on. It has no idea which conversation is on screen, so `ChatScreen` registers the open strip with `ComposerPasteTarget` on appear and takes it back on disappear (only the registrant may clear the slot: SwiftUI does not promise the leaving screen's `onDisappear` runs before the arriving screen's `onAppear`).

**It catches image-ONLY clipboards, by construction and not by choice.** When the board also carries text — or a URL, which a rich-text range with an inline image typically adds — the field answers YES, *it* becomes the target, it inserts the text, and nothing above it is ever consulted. **An ancestor can never outrank the first responder.** Handling that case means owning the first responder, i.e. replacing the field with a `UITextView` subclass that decides for itself (image → stage, text → `super`, both → both). That is the only mechanism that also gets ⌘V, and it is a deliberate non-goal here: it would have to re-implement the 1…6-line autosizing whose height the dock reports to the web as the transcript's bottom obstruction, the 17pt + 13pt = 48pt row math (UITextView's default `textContainerInset` and `lineFragmentPadding` break both), the placeholder, the `@FocusState` two-way bridge that drives the pill's padding animation and `-baybo-demo-keyboard`, and the CJK marked-text ordering in `clearField`. The `+` panel's Paste row is the affordance that works for every clipboard shape; this one is the gesture people reach for first.

**Never forward `paste:` to `super`.** `UIResponder`'s default answers YES for any action the class merely *implements*, so an `AppDelegate` that overrides `paste(_:)` and delegates the question upward claims every paste in the app — including the plain text ones the field handles perfectly, which then vanish into an attachment strip instead of the draft. `ComposerPasteTargetTests` pins that, along with the two other answers that keep this narrow: no composer registered → no, and no image on the board → no.

**The read here is AUTHORISED, and that is why it is synchronous.** iOS's own Paste command is the user intent it looks for, so the pull is silent — and the permission belongs to the *interaction*, so hopping to another task to read it later is how you land outside the window it granted. `stagePasteboard(authorized:)` carries that distinction: authorised reads stay on the main actor, the `+` row's unauthorised read goes to a detached task because it can be sitting behind a modal alert.

The UI-tier case (`testLongPressPasteReachesTheResponderChain`) is the only thing that can see the walk happen at all — the unit suite asks the delegate directly, so it stays green even if nothing ever reaches it. It **empties the real clipboard first**, and that is the mechanism rather than hygiene: the walk only continues when the field declines, and Simulator.app syncs the host Mac's clipboard by default, so whatever the developer last copied would otherwise make the field take the paste. It passed on a virgin device and failed on the same device an hour later before that line existed.

**The dock chain must never clip** (`ComposerDock`). The panel's box is entirely NEGATIVE in dock space (`AttachMenuPanel.box` → `y ∈ [-102, -10]` for two rows, `[-148, -10]` with Paste — it grows upward from a fixed floor) so it floats above the dock's top edge, clear of everything the dock grows upward; `composerVeil`'s solid tail and the pill's ambient shadow draw outside those bounds too. `50b4e33f` put a `.clipped()` on that chain and erased the panel outright — and because a SwiftUI clip discards **paint only**, its layout, hit region and accessibility frame all stayed correct, so `AttachMenuTests`' geometry cases and `ComposerAttachUITests`' `exists`/`isHittable`/frame assertions were *all* green while the `+` dimmed the screen and showed nothing (the invisible rows stayed tappable, firing a picker out of nowhere). The collapse an expanded HTML preview asks for is `opacity` + a zero `frame`, never a clip.

That is why the chain lives in `ComposerDock` — store-free and generic — rather than inline in `ChatScreen.body`, which needs a `ChatStore` and a live `TranscriptHost` webview to instantiate and so was reachable only from XCUITest. `ComposerDockTests` renders the real dock with the real panel through `ImageRenderer` and counts ink pixels in the panel's box; putting the `.clipped()` back fails it in ~1.6s. **Only pixels catch this class** — do not "cover" it with another geometry or accessibility assertion.

That count is a **UI limit, not a wire cap** — it bounds thumbnails, uploads and strip tiles, and the gateway enforces its own per-message attachment cap independently (`MAX_MESSAGE_BATCH_ATTACHMENTS`, which the singular `Frame::Message` path validates too). The BYTE cap is separate and is the gateway's: `ChatStore.maxAttachmentBytes` mirrors `MAX_BLOB_BYTES` (100 MiB) and there is exactly one of it.

### A Files URL is security-scoped

**Wrap every read of a `fileImporter` URL in `startAccessingSecurityScopedResource()` / `stopAccessingSecurityScopedResource()`** — the size probe and the upload each take their own balanced bracket (the upload's has to span the `await`, and a retry re-acquires). Miss it and an iCloud / Files-provider document reads as silently EMPTY: no error, no bytes, a zero-length blob on the wire.

The size comes from `URLResourceValues.fileSize` **before** anything is read, so an over-cap pick is refused without materialising 100 MiB to find out.

### The mime decides everything downstream

- A Files pick's mime is `UTType(filenameExtension:)?.preferredMIMEType`. When the OS can't name the extension, a short map of unambiguously-UTF-8 ones (`rs`, `toml`, `yml`, `log`, `env`, …) sends `text/plain` — `crates/llm` only inlines a text-like mime, so an octet-stream `.rs` reaches the model as a placeholder instead of its source. Anything that might be binary keeps `application/octet-stream`.
- A photo's is the **magic-byte sniff first** (JPEG / PNG / GIF / WebP / the HEIF `ftyp` brands), with the picker item's `supportedContentTypes` filling in only for bytes the sniff can't name. `supportedContentTypes` lists what the item CAN be delivered as and `loadTransferable` promises nothing about returning the first entry — when PhotosUI transcodes for compatibility, the declared type and the bytes that actually arrived disagree, and the mime is what the gateway stores and what the provider is handed.
- A pasted image's is the same sniff, with the clipboard flavour as the hint — the paste path calls the identical `photoMime(declared:data:)`, so it inherits the rule rather than restating it. What paste adds is a **format election before that**: `SystemPasteboard` hands over the ORIGINAL bytes for the flavours the sniff already names (`public.png` / `public.jpeg` / `public.heic` / `public.heif` / GIF / WebP, in that order), and only re-encodes to PNG when the item offers nothing else. Both halves matter. Re-encoding unconditionally — which is what `UIPasteboard.image` forces, since it hands back a decoded `UIImage` — would replace the user's HEIC with a much larger PNG under a mime that no longer describes what they copied. Not re-encoding at all would ship `public.tiff`, which a copy originating on macOS really does carry: TIFF is in neither `crates/llm`'s image whitelist nor the FROZEN `mime_extension` disk-layout table, so those bytes would store a plain file card and reach the model as a placeholder. Teaching the storage table about `image/tiff` is a silent data migration, not a fix (`mime_extension` in `crates/storage/src/sqlite/blob.rs` decides a blob's on-disk filename, and adding a row to it has already deleted another live row's bytes once) — so the normalisation belongs client-side, here.
- **The wire KIND is derived from that mime, never from which picker the file came through** (`StagedAttachment.kind(forMime:)`): an image picked in Files is still `image`, `audio/*` is `audio`, and everything else — INCLUDING `video/*`, which has no kind of its own — is `file`.

This is also why the file tile middle-truncates its name (`.truncationMode(.middle)`): the extension has to stay visible, because the mime it implies is what decides whether the model can read the file at all.

### Everything uploads off a path

**Every** staged pick uploads with `blob_upload_file(path, mime, progress)` — the bytes never cross the FFI, so a 100 MiB attachment is never held whole in memory. A Files pick already has a path; a photo does not (PhotosUI hands over a `Data`), so its bytes are **spooled to `<tmp>/baybo-compose/<run id>/<pick id>.<ext>`** at staging and the tile keeps only that file. A pasted image has no path either, and takes that photo branch verbatim — `stagePasteboard` reduces to `admitPhoto()` + `acceptPhoto(id:data:declaredMime:)`, so the spool, the `SpoolFile` ownership, the downsampled thumbnail, the byte cap and `pumpUploads` are all inherited rather than re-derived. Holding the `Data` on the item instead — which is what a retry would need — is ten full-size encoded picks alive for as long as the strip is up, on top of ten decoded thumbnails: a foreground jetsam on realistic ProRAW/panorama picks. The thumbnail is decoded **downsampled through ImageIO** off the spooled file (`CGImageSourceCreateThumbnailAtIndex`, 256px) rather than with `UIImage(data:)`, which would keep the encoded bytes alive behind the image.

### The draft belongs to the SURFACE, not to the composer frame

`ComposerStaging` is owned by whichever surface the draft belongs to — `ChatStore.staging` for a conversation, `IssueStore.staging` for a project card — and holds **the whole unsent draft**, the typed text as well as the strip. Neither dock keeps `@State` of its own for either.

The machine reaches its surface through `ComposerHost` and nothing else: `draftKey` (which root and which id) and `notice` (the line the strip raises and retracts). That is all it ever wanted from `ChatStore`, and it is why the card page could grow the same strip in a day; see [projects.md](projects.md) §3.3. There is deliberately **no `send`** on that seam — the two sends differ essentially (outbox and connection gate versus one REST comment) — so the door out is `claimSend() -> ComposerPayload?`, which is also where the send gate lives: a surface that read `staged.compactMap(\.blobId)` for itself would ship a message minus every pick still uploading, silently.

**Two draft roots, never one.** `drafts/` is the chats'; `card-drafts/` is the cards'. `AppStore.unsentDraftSessionId` enumerates the chat root and treats an unlisted, outbox-free directory there as the abandoned new chat the compose button resumes — a card's comment draft filed beside them would open as a conversation, and `SessionIndex`'s sweeps would delete it under chat rules. `DraftStore.sessionIds` scans `.chat` and only `.chat`; `deleteAll` takes both.

**The card has no outbox, so only a landed comment discards.** `IssueStore.comment` answers `Bool` and the dock keeps its text and its tiles on a `false`: the picks are uploaded blobs, and clearing the strip on a failure strands files the operator cannot get back.

**`ComposerView.onDisappear` cannot be a lifetime hook.** The composer is docked in `ChatScreen`'s `.safeAreaInset`, and every `fullScreenCover` the chat presents — the image viewer, the video player — tears that content down and puts it straight back. Reclaiming there meant a user with three photos mid-upload who tapped an image in the transcript came back to an *empty* strip with the typed text still in the field: three uploads cancelled, three spools unlinked, no notice, and a send that shipped the message MINUS its attachments — precisely what the failed-upload blocker exists to prevent. (The abandoned uploads still complete and mint permanent orphan blobs; nothing sweeps chat blobs.) A `.sheet` — QuickLook, the share sheet, the message index — leaves the presenter on screen and fires no teardown at all, but the fix does not depend on knowing which is which: no presentation changes a route, so none of them can name a conversation as left.

The demo seed (`-baybo-demo-compose`) is planted in `ComposerStaging.init` rather than on the composer's appear for the same reason: seeded per FRAME, it silently refilled the strip a cover had just emptied — the bug's own fixture hiding it. It APPENDS, because the strip it lands in may already have been restored from a draft.

### Leaving is not discarding — the draft on disk

**Exactly two things end a draft: the message is sent, and the conversation is deleted.** Both go through `ComposerStaging.discardDraft()`. Everything else — a cover, backing out to the list, an LRU eviction, backgrounding, a jetsam, a relaunch — is a checkpoint, and the draft comes back on the next visit.

It persists through `DraftStore`, one directory per session under `Application Support/baybo/drafts/`, a sibling of the transcript mirror and the send outbox:

```
drafts/<session id>/draft.json      { text, attachments: [...] }
drafts/<session id>/<pick id>.thumb the staged tile's image
drafts/<session id>/<pick id>.src   the pick's bytes, WHILE it has no blob
```

**What is kept per pick follows from what a SEND needs.** A pick that reached its blob keeps only a thumbnail — `AttachmentRef` is the whole message from there, and the bytes are the gateway's. A pick still queued, uploading or failed keeps the bytes too, because nothing else holds them. That second case is not hypothetical: **offline is exactly when every upload fails**, and without it the picks vanish from a draft that still shows the text that went with them. On the next open they re-queue themselves, which is usually where the network is again.

Keeping them is affordable because it is a **hard link**, not a copy: `retainSource` links the composer's temp spool into the draft directory (O(1), no second copy of a 100 MiB pick), and the restore `relink`s it back into *this* run's spool directory and hands the strip an ordinary `SpoolFile`. A restored pick is therefore indistinguishable from a fresh one — same ownership, same reclamation, same upload path — and, crucially, **nothing on the strip ever holds a path under `drafts/`**. That is what lets `DraftStore.write` prune the directory on every write: the upload streaming a restored pick is reading its own spool, and unlinking one name of a hard-linked inode takes no byte from the other. (Pruning a path an upload *is* reading is the exact hazard `SpoolFile` exists to avoid — see the next section.)

A **Files** pick with no blob yet is the one exception: its bytes are the user's own document, never ours to copy, so the draft keeps a security-scoped bookmark instead. The bookmark is minted ONCE, when the pick is admitted, and carried on the tile (`StagedAttachment.bookmark`) — re-deriving it per write meant that a document which had since become unreachable failed `url.bookmarkData()` and the pick vanished from the record while its tile sat on the strip.

A bookmark whose document has since moved or been deleted is the only way a pick can fail to restore. The composer says so (`chat.draftAttachmentsLost`) rather than letting a file the user remembers attaching go missing in silence — and it **rewrites the record**, because a restore dirties nothing on its own: the dead entry would otherwise survive every visit and re-raise a line nobody can act on, and a draft whose picks ALL died would become an empty record that no longer deletes itself, which is precisely what compose resumes. The line itself is raised a turn late, twice over: this machine is built lazily from `ComposerView.init` (inside a view update, on a `ChatStore` SwiftUI is already rendering), and `ChatScreen.onAppear`'s `connectIfNeeded()` clears `notice` on its way out.

Writes are debounced 400 ms; every exit the app can see coming flushes instead of waiting — `ChatStore.leaveChat`, `ChatStore.evict`, and `AppStore.willLeaveForeground` off any non-`.active` scene phase. The tree is excluded from backup, like the blob cache and for the same reason.

**Deleting the conversation has to reach the in-memory copy too.** A resident `ChatStore` holds the draft in memory and writes it back on its next flush — and `AppStore.requestDelete` *evicts*, which flushes — so deleting the file alone puts the draft straight back on disk. The resurrected file, having no row, then reads as an unsent new-chat draft, and every subsequent compose lands in the conversation the user deleted.

The registry is the deletion authority but cannot reach the stores, so it names what it removed: **`SessionIndex.onSessionsRemoved`**, fired by `beginHide`, `beginHideMany` and the merge-driven sweep, with `AppStore` discarding each named store's draft. One mechanism covers all three, including the one with no local action behind it at all — a conversation deleted **on another client** arrives as an ordinary list merge.

Two consequences are accepted rather than fixed:

- **A failed delete does not bring the draft back.** `beginHide` deletes optimistically and `rollBackHide` restores only the row, exactly as it does for the transcript mirror — the difference being that a mirror is rebuildable from the gateway and a draft is not. The user confirmed a destructive action; buying the alternative costs a staged-delete mechanism the rest of the registry does not have.
- **A landed pick's spool stays in `<tmp>` for the `ChatStore`'s lifetime.** Leaving used to release the strip; now only a send, a delete or dropping the store does. The draft record gives its retained copy back the moment the blob lands, but the strip still holds the `SpoolFile` — so the bytes are pinned while the conversation stays resident (bounded by `maxResidentStores`, reclaimed by the launch sweep and by iOS under storage pressure).

**A machine whose store is dropped is RETIRED, not just released** (`ComposerStaging.retire`, from `ChatStore.evict` and `disconnect`). An upload Task captures the machine strongly and `blob_upload_file` has no cancellation hook, so it outlives its `ChatStore` for as long as anything is on the wire — while re-opening the conversation builds a second machine over the same `drafts/<session id>/`. The zombie's terminal write passes its own `holds(id)` guard (the tile IS still on its strip) and would restore a draft the live machine has since sent. Retiring flushes once and then refuses every later write. It is also why a restored pick's spool is named with a **fresh uuid** rather than the pick's own preserved id: two machines deriving the same path would hold two independent `SpoolFile`s over it, and the first `deinit` would unlink the bytes the other is streaming.

### Compose returns to the draft it left

`AppStore.startNewChat` resumes an unsent draft session instead of always minting a uuid. It has to: a draft session has **no registry row and no gateway row**, so its uuid is the only handle that exists anywhere, and minting a fresh one would strand what the user wrote in a conversation nothing on the device can ever name again. At most one can ever be waiting, because resuming is what stops compose from minting another; emptying the composer retires it (`DraftStore.write` deletes an empty draft) and the next compose mints as before.

"Unsent draft session" means *a draft on disk that has never been **sent from***, and that takes **two** proofs. A registry row is the ordinary one — a row means the conversation exists and is reachable from the list, where its own draft comes back on the tap. But the row is written by `ChatStore.dispatchSend` only after `ensureRemoteSession()` succeeds, so a first send made **offline** leaves the conversation row-less with its message queued; type one more line into it and compose would re-open the conversation you just sent to, believing it new. A persisted **outbox** is the second proof, and it is exactly as durable as the draft it is being compared against.

### The spool is owned, not deleted

A spool belongs to a **`SpoolFile`, which unlinks it in `deinit`** — there is no "delete the file" call anywhere in the composer, and a Files pick carries no `SpoolFile` at all (the user's own document, never ours). That ownership answers two questions at once:

- **When does an abandoned strip's disk go back?** When the tiles do — on the ✕, and on the send or the delete that ends the draft (`ComposerStaging.discardDraft()`). Dropping the `ChatStore` itself (LRU eviction, logout's `resetChatStores`) reclaims the same way, one reference further out. Nothing else can: the name is a UUID nothing will ever ask for again, unlike the digest-keyed `baybo-preview` / `baybo-deck-share` caches, which are bounded by the number of distinct blobs and hit again on re-open. A leftover here is pure dead weight, up to 100 MiB of it per pick. Leaving the conversation reclaims *nothing*, deliberately — the strip is still a draft (above), and what the draft is keeping under `drafts/` is a hard link that costs no extra bytes while the spool is alive anyway.
- **What if the upload is still reading it?** Then the upload is holding the same `SpoolFile` (`upload` keeps `source` on its frame for the whole call), and the file outlives the tile by exactly that long. Unlinking on removal instead raced the Rust side's **two opens** — the hash pass, then the body reader — so a ✕ tapped mid-upload either failed the upload with a read error or, landing after both opens, let it silently succeed.

**A kill, a crash or a jetsam runs no `deinit`**, so `StagedAttachment.sweepAbandonedSpools()` runs once at launch (`BayboApp`) and deletes every spool directory except this run's. Per-RUN directories are what make that safe without bookkeeping: everything this process spools is inside the one child the sweep skips, so it can never reach a file a live upload is reading.

### Removing a tile cancels its work

`ComposerStaging.remove` cancels the pick's `work` handle (its PhotosUI delivery, or its upload) instead of yanking the file out from under it, and **every terminal effect is conditional on the tile still being in the strip** — the state write (`update` no-ops on a missing id) *and* the failure notice (`publishNotice`). Without the second, a ✕ on a spinning tile still put "Send failed: request timed out" in the dock seconds later, for an attachment the user could no longer see; the same went for a photo that failed to load or came back over-cap after its tile was gone.

**A notice the strip published is also RETRACTED when its tile leaves** (`noticeOwner` → `retractNotice`). The ✕ is the action that red line offers, so taking it has to silence it — otherwise "Send failed: …" sat over an empty strip until the user dismissed it by hand. The send gate's own line goes through the same owner (`noteBlocked` names the tile holding the message up), and `leaveConversation()` retracts whatever is left, so re-entering a chat can't open on a line about a failure that is stale by then — the tile it named still carries its own red retry affordance, and `noteBlocked` raises the line again the moment a send runs into it. The retraction is conditional on the published text still being the one on the dock: `notice` belongs to the whole screen — a model failure and a failed approval land on it too — and a ✕ may only take back what the strip put there.

Cancellation stops a photo's `loadTransferable`, but **not an upload already on the wire**: the generated UniFFI async binding has no cancellation hook, so `blob_upload_file` runs to completion regardless. A pick removed mid-upload therefore still mints its blob on the gateway, and nothing sweeps chat blobs — that orphan is permanent. What cancellation buys is that the result reaches neither the strip nor the notice line; plumbing a real abort would have to start in `ffi/`.

`ChatStore.maxConcurrentUploads` (2) bounds how many run at once — the rest sit `queued`. Ten in parallel is ten sockets on one uplink and ten 100ms progress tickers hopping to the main actor, which defeats the coalescing the tick interval exists to provide.

**That budget counts what is on the WIRE, not what is on the strip.** Because a removed tile's upload keeps streaming (above), `staged.filter(isUploading)` under-counts by exactly the zombies: stage four with two removed mid-flight and the first completion saw an empty strip, released BOTH queued picks, and ran three uploads at once — each with its own 100ms ticker still hopping to the main actor. `uploadsInFlight` is incremented by `pumpUploads` and decremented by the same task after its call returns.

Progress presents exactly like a download does: an indeterminate spinner with the **byte counter beside it as the real progress** (`884 KB / 2.3 MB`), on the tile's second line.

### Send gating, and the retry

Staged picks count as a draft, so an attachment-only send works with an empty field. Send is **gated** while anything is still on its way to the gateway and **blocked** while anything FAILED — a failed pick carries no blob, and shipping the message without it would drop the user's file in silence.

**A pick takes its slot in the strip the moment it is ADMITTED**, in the `queued` state, not when its bytes finish arriving — `StagedAttachment.blocker` reads the strip, so a pick still inside `loadTransferable` has to be in it. Appending only after that `await` meant a send fired mid-batch saw neither an uploading nor a failed item: it shipped with whichever refs happened to be ready, cleared the strip, and the rest of the batch landed as ghost tiles on the *next* message.

With multi-select, "remove it and pick again" is not a cure, so **a failed tile retries on tap** (its ✕ still removes it) — re-reading the file the `Source` points at. The retry is *claimed* (`claimRetry`) on the current array element before any await, because two taps in one frame both read the same render snapshot and would otherwise mint two blobs on the gateway and race over which one the message references.

### An attachment-only send still gets a chat-list preview

The gateway's `last_message_preview` is `None` for a media-only turn, and `merge` keeps a local preview over a nil remote one — so `SessionIndex.recordUserSend` describes the attachments itself (the filenames, or the count when nothing is named), run through the same `previewText` truncation as every other capture. `userText` is left alone: it is the bold line's fallback until the title pass runs, and a filename names no conversation.

### Known gap: outbound `duration_ms`

`WireAttachment.duration_ms` — what the audio card's resting meta line and the video tile's chip read — is always `None` on a send: the FFI's `AttachmentRef` record (`app/ios/ffi/src/api.rs`) has no such field, and `From<AttachmentRef> for WireAttachment` hardcodes it. A user can now attach an `m4a` or an `mp4`, so probing it locally (`AVURLAsset`) is worth doing — but it needs the FFI record to carry it first, or the optimistic bubble would show a length that vanishes the moment the row comes back from a sync.
