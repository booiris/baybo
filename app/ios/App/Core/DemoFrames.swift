#if DEBUG
    import Foundation
    import UIKit

    /// `-baybo-demo-frames` (DEBUG launch arg): feeds one canned turn through
    /// the transcript bridge — user bubble, thinking trace, a tool call,
    /// streamed markdown answer, terminal message — so the chat rendering is
    /// screenshot-verifiable headlessly on a simulator with no gateway. Pushes
    /// ride the bridge's pre-ready buffer, so timing races with page load are
    /// harmless.
    extension ChatStore {
        private static let demoFramesArg = "-baybo-demo-frames"
        private static let demoAttachmentsArg = "-baybo-demo-attachments"
        private static let demoDownloadArg = "-baybo-demo-download"
        private static let demoImagesArg = "-baybo-demo-images"
        private static let demoIndexArg = "-baybo-demo-index"
        static let demoApprovalArg = "-baybo-demo-approval"
        static let demoSwitchArg = "-baybo-demo-switch"
        static let demoHtmlArg = "-baybo-demo-html"

        /// Flags that push a canned TURN — rows that look durable but are backed
        /// by no gateway — into a session that is only ever a local draft.
        private static let cannedTurnArgs = [
            demoFramesArg, demoAttachmentsArg, demoImagesArg, demoIndexArg, demoApprovalArg,
            demoSwitchArg, demoHtmlArg,
        ]

        /// True while such a fixture is driving. `ChatStore.requestSync` reads it:
        /// a draft's baseline `sync_page` is empty and REPLACES the thread, which
        /// is exactly right for a real draft (nothing durable exists yet, and the
        /// optimistic bubble is preserved) and exactly wrong for a demo, whose
        /// canned rows are neither optimistic nor durable and so get wiped.
        ///
        /// It only ever bit us on a slow machine: the webview's mount sync and
        /// the demo's timer race, and on a dev Mac the sync lands FIRST (wiping
        /// an empty thread, harmlessly) while a loaded CI runner boots the
        /// webview slowly enough that the canned turn is replayed first and the
        /// sync then deletes it. The turn simply vanished, and the UI smokes read
        /// it as the product failing to render.
        static var isDrivingCannedTurn: Bool {
            let args = ProcessInfo.processInfo.arguments
            return cannedTurnArgs.contains(where: args.contains)
        }

        /// Natural pixel sizes the demo images decode to — a spread that makes a
        /// wrongly-reserved box obvious: a portrait that grows its row, a banner
        /// that shrinks it, a thumbnail under every cap, a square the height cap
        /// clamps. The declared `size` is nominal (the fake bytes are a flat PNG).
        private static let demoImageSizes: [(id: String, width: Int, height: Int)] = [
            ("sha256:demoimg1.tok", 768, 1024),
            ("sha256:demoimg2.tok", 1600, 400),
            ("sha256:demoimg3.tok", 80, 60),
            ("sha256:demoimg4.tok", 900, 900),
        ]

        private static var demoImageAttachments: [[String: Any]] {
            demoImageSizes.map {
                [
                    "kind": "image", "blob_id": $0.id, "mime_type": "image/png",
                    "size": $0.width * $0.height * 4,
                ]
            }
        }

        /// `-baybo-demo-images` (DEBUG): one agent turn carrying the four images
        /// above and a text row UNDER them — that row's y-position is the whole
        /// test. A first run (empty mirror) paints the 12rem loading tiles, then
        /// each image releases to its real height and shoves the row around; the
        /// sizes it records mean a SECOND run of the same session reserves each
        /// box up front and the row must not move at all. Runs with no gateway:
        /// `requestBlob` is served locally (see `serveDemoImageIfRequested`).
        func startDemoImagesIfRequested() {
            guard ProcessInfo.processInfo.arguments.contains(Self.demoImagesArg) else { return }
            // ONLY the throwaway demo session: this feeder both persists a turn
            // into the session's durable mirror and writes its registry row, so
            // pointing it at a real conversation (`-baybo-open-session`) would
            // corrupt one.
            guard sessionId == AppStore.debugSessionId else { return }
            // Register the session the way a real send would: the mirror holding
            // the recorded image sizes dies with its chat-list row, and an
            // unregistered session has none. (On a BOUND sim the next list merge
            // drops this local-only row anyway — remote wins for existence — and
            // takes the mirror with it, which is why the relaunch half of this
            // check wants an unbound sim; see app/ios/docs/testing.md.)
            SessionIndex.shared.recordUserSend(sessionId: sessionId, text: "把那几张图发我")
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(400))
                pushDemoUserSent(msgId: "demo-img-user", text: "把那几张图发我")
                pushDemo(["kind": "turn_state", "active": true])
                try? await Task.sleep(for: .milliseconds(400))
                pushDemo([
                    "kind": "message", "role": "assistant", "content": "都在这儿了。",
                    "platform_msg_id": "demo-img-1", "ordinal": 1,
                    "attachments": Self.demoImageAttachments,
                ])
                pushDemo([
                    "kind": "message", "role": "assistant",
                    "content": "ANCHOR — this row must not move between the pre-load and post-load frames.",
                    "platform_msg_id": "demo-img-2", "ordinal": 2,
                ])
                pushDemo(["kind": "turn_state", "active": false])
            }
        }

        /// One canned turn of the index demo: the user's send and the agent's
        /// answer to it, placed at a wall-clock moment relative to the run.
        private static let demoIndexTurns:
            [(
                prompt: String, reply: String, daysBack: Int, hour: Int, minute: Int,
                attachment: Bool
            )] = [
                (
                    "Summarize what landed in the release last night",
                    "Nine merges: the sync cursor rewrite, two crash fixes, and the new deck card sizes.",
                    1, 9, 15, false
                ),
                (
                    "Which crates changed the wire format?",
                    "**wire** and **device-proto**. `Frame::Message` gained nothing; the change is all on `SubscribeState`.",
                    1, 14, 40, false
                ),
                (
                    "Walk me through why the transcript webview is shared across every conversation instead of one per session, and what that costs us",
                    "One WKWebView means one web-content process and one bundle parse for the app's whole bound lifetime — opening a chat never boots a runtime. The cost is that every piece of per-session state has to be reset on retarget.",
                    1, 21, 5, false
                ),
                (
                    "Any regressions from that merge?",
                    "None on the phone. The tmux render job flaked twice, which it does under load.",
                    0, 8, 30, false
                ),
                // An attachment-only send: no prompt text at all, which is the
                // row the index has to describe without any words of its own.
                ("", "That's the character card — I've folded it into the deck bundle.", 0, 10, 12, true),
                (
                    "Draft the PR body for it",
                    "Done — three paragraphs, with the manual verification steps under their own heading since CI never sees this path.",
                    0, 11, 47, false
                ),
            ]

        /// Ordinals start well above zero so `has_more_older` below is TRUE
        /// rather than a lie — the sheet's "load earlier" row only exists when
        /// the thread genuinely has more behind it.
        private static let demoIndexFirstOrdinal = 101

        /// `-baybo-demo-index` (DEBUG, with `-baybo-open-chat`): a thread long
        /// enough to open the header's message index — six of the user's own
        /// sends (one attachment-only, one far past the row's text cap) with the
        /// agent's replies, split across yesterday and today so the sheet's day
        /// headers render. `-baybo-demo-frames` pushes a single send, which is
        /// below the index button's own gate.
        ///
        /// Rides ONE synthesized `sync_page` — the frame the gateway path
        /// produces — because the index's clock and day key come from each row's
        /// `created_at`, and a live `message` frame has no time field at all.
        func startDemoIndexIfRequested() {
            guard ProcessInfo.processInfo.arguments.contains(Self.demoIndexArg) else { return }
            // A canned page REPLACES the thread and is persisted into that
            // session's mirror, so pointing this at a real conversation
            // (`-baybo-open-session`) would overwrite one.
            guard sessionId == AppStore.debugSessionId else { return }
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(400))
                let rows = Self.demoIndexRows()
                pushDemo([
                    "kind": "sync_page",
                    "rows": rows,
                    "since_ordinal": NSNull(),
                    "next_cursor": Self.demoIndexFirstOrdinal + rows.count - 1,
                    "rebased": false,
                    "oldest_ordinal": Self.demoIndexFirstOrdinal,
                    "has_more_older": true,
                ])
            }
        }

        private static func demoIndexRows() -> [[String: Any]] {
            var rows: [[String: Any]] = []
            var ordinal = demoIndexFirstOrdinal
            for turn in demoIndexTurns {
                guard
                    let asked = demoIndexDate(
                        daysBack: turn.daysBack, hour: turn.hour, minute: turn.minute)
                else { continue }
                var asks: [String: Any] = [
                    "id": "m\(ordinal)", "ordinal": ordinal, "kind": "message", "role": "user",
                    "text": turn.prompt,
                    "platform_msg_id": "demo-index-u\(ordinal)",
                    "created_at": demoIndexStamp.string(from: asked),
                ]
                if turn.attachment {
                    asks["attachments"] = [demoFileAttachments[1]]
                }
                rows.append(asks)
                ordinal += 1
                rows.append([
                    "id": "m\(ordinal)", "ordinal": ordinal, "kind": "message",
                    "role": "assistant", "text": turn.reply,
                    "created_at": demoIndexStamp.string(from: asked.addingTimeInterval(45)),
                ])
                ordinal += 1
            }
            return rows
        }

        /// A time set on a calendar DAY, never an offset from now: a fixture that
        /// backed up "26 hours" would put both of its days on the same date when
        /// it ran in the evening, and the day headers are the point here.
        private static func demoIndexDate(daysBack: Int, hour: Int, minute: Int) -> Date? {
            let calendar = Calendar.current
            guard let day = calendar.date(byAdding: .day, value: -daysBack, to: Date()) else {
                return nil
            }
            return calendar.date(bySettingHour: hour, minute: minute, second: 0, of: day)
        }

        private static let demoIndexStamp = ISO8601DateFormatter()

        /// Serve a demo image's bytes with no gateway and no blob leg: a flat PNG
        /// at the declared pixel size, behind a delay long enough to screenshot
        /// the layout BEFORE the bytes land — which is exactly the frame the
        /// reserved box has to already be right in.
        func serveDemoImageIfRequested(id: Int, blobId: String) -> Bool {
            guard let demo = Self.demoImageSizes.first(where: { $0.id == blobId }) else {
                return false
            }
            Task { @MainActor in
                try? await Task.sleep(for: .seconds(2))
                guard let png = Self.demoImagePng(width: demo.width, height: demo.height) else {
                    return
                }
                pushDemoBlobResult(
                    id: id, dataBase64: png.base64EncodedString(), mimeType: "image/png")
            }
            return true
        }

        /// Scale 1 is load-bearing: the renderer defaults to the screen's (3x),
        /// which would decode three times the size the box was reserved at.
        private static func demoImagePng(width: Int, height: Int) -> Data? {
            let size = CGSize(width: width, height: height)
            let format = UIGraphicsImageRendererFormat.default()
            format.scale = 1
            return UIGraphicsImageRenderer(size: size, format: format).image { ctx in
                UIColor(white: 0.85, alpha: 1).setFill()
                ctx.fill(CGRect(origin: .zero, size: size))
                UIColor(white: 0.35, alpha: 1).setStroke()
                let diagonal = UIBezierPath()
                diagonal.move(to: .zero)
                diagonal.addLine(to: CGPoint(x: width, y: height))
                diagonal.lineWidth = CGFloat(max(width, height)) / 40
                diagonal.stroke()
            }.pngData()
        }

        /// `-baybo-demo-download` (with `-baybo-demo-attachments`): push the
        /// `fileState` messages a real download would, so the card's idle →
        /// loading (spinner + byte counter) → ready transition is
        /// screenshot-verifiable with no gateway and no blob leg. Drives the
        /// first FILE card and the VIDEO tile together — the tile's centered
        /// determinate ring and corner byte counter walk the same reducer.
        /// The video's `ready` makes its card request a poster, which
        /// `serveDemoVideoPosterIfRequested` answers locally.
        private func driveDemoDownloadIfRequested() async {
            guard ProcessInfo.processInfo.arguments.contains(Self.demoDownloadArg) else { return }
            let cards = [Self.demoFileAttachments[0], Self.demoVideoAttachment]
            try? await Task.sleep(for: .milliseconds(1500))
            for step in 1...6 {
                for card in cards {
                    let blobId = card["blob_id"] as? String ?? ""
                    let total = card["size"] as? Int ?? 0
                    pushDemoFileState(
                        blobId: blobId, state: "loading",
                        loaded: UInt64(total * step / 8), total: UInt64(total))
                }
                try? await Task.sleep(for: .milliseconds(700))
            }
            for card in cards {
                pushDemoFileState(blobId: card["blob_id"] as? String ?? "", state: "ready")
            }
        }

        /// Spread the file chip has to survive: a long name that must clip, a
        /// nameless blob that falls back to its mime, a sub-kilobyte file and a
        /// multi-megabyte one. A file chip renders straight from the frame (no
        /// blob fetch), so this is screenshot-verifiable with no gateway — the
        /// `image` kind would need a live blob leg.
        private static let demoFileAttachments: [[String: Any]] = [
            [
                "kind": "file", "blob_id": "sha256:demo1.tok",
                "mime_type": "application/pdf", "size": 2_413_512,
                "filename": "baybo-architecture-review-2026-Q3-final.pdf",
            ],
            [
                "kind": "file", "blob_id": "sha256:demo2.tok",
                "mime_type": "image/svg+xml", "size": 24_190,
                "filename": "bg_character_card.svg",
            ],
            [
                "kind": "file", "blob_id": "sha256:demo3.tok",
                "mime_type": "application/zip", "size": 812,
            ],
        ]

        /// The optimistic user-echo path's own fixture: `userSent` hands the
        /// staged refs straight to the webview, a different entry point from
        /// the durable `message` frame the rest of this feeder pushes. Same
        /// blob as the agent's card below, so the two cards' shared per-blob
        /// state is visible too.
        private static let demoSentAttachment = AttachmentRef(
            kind: .file, blobId: "sha256:demo2.tok", mimeType: "image/svg+xml", size: 24_190,
            filename: "bg_character_card.svg")

        /// An audio card (play affordance once ready) and a video tile
        /// (centered disc + corner chip). Both render straight from the frame;
        /// real playback needs real bytes, so tapping them here only exercises
        /// the download affordance.
        private static let demoAudioAttachment: [String: Any] = [
            "kind": "audio", "blob_id": "sha256:demoaudio.tok",
            "mime_type": "audio/mpeg", "size": 3_481_600,
            "filename": "morning-light-sketch.mp3",
            "duration_ms": 203_000,
        ]

        private static let demoVideoAttachment: [String: Any] = [
            "kind": "file", "blob_id": "sha256:demovideo.tok",
            "mime_type": "video/mp4", "size": 24_804_352,
            "filename": "landing-flow-capture.mp4",
            "duration_ms": 83_000,
        ]

        /// Stand-in bytes for a demo blob a share/preview/player wants
        /// materialised. The media ones are REAL (ffmpeg-made 2s clips — a
        /// gray 160×90 H.264 and a 440 Hz MP3 tone, base64-embedded; DEBUG
        /// source only), so the native players present their genuine chrome
        /// and actually play headlessly.
        static func demoMaterializeBytes(blobId: String) -> Data? {
            let pdfId = demoFileAttachments[0]["blob_id"] as? String
            let audioId = demoAudioAttachment["blob_id"] as? String
            let videoId = demoVideoAttachment["blob_id"] as? String
            switch blobId {
            case pdfId:
                return Data("%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n".utf8)
            case audioId:
                return Data(base64Encoded: demoMp3Base64)
            case videoId:
                return Data(base64Encoded: demoMp4Base64)
            default:
                return nil
            }
        }

        private static let demoMp4Base64 =
            ""
                + "AAAAIGZ0eXBpc29tAAACAGlzb21pc28yYXZjMW1wNDEAAAOFbW9vdgAAAGxtdmhkAAAAAAAAAAAAAAAAAAAD6AAAB9AAAQAA"
                + "AQAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAABAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                + "AAAAAgAAAq90cmFrAAAAXHRraGQAAAADAAAAAAAAAAAAAAABAAAAAAAAB9AAAAAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAA"
                + "AAAAAAABAAAAAAAAAAAAAAAAAABAAAAAAKAAAABaAAAAAAAkZWR0cwAAABxlbHN0AAAAAAAAAAEAAAfQAAAAAAABAAAAAAIn"
                + "bWRpYQAAACBtZGhkAAAAAAAAAAAAAAAAAAAwAAAAYABVxAAAAAAALWhkbHIAAAAAAAAAAHZpZGUAAAAAAAAAAAAAAABWaWRl"
                + "b0hhbmRsZXIAAAAB0m1pbmYAAAAUdm1oZAAAAAEAAAAAAAAAAAAAACRkaW5mAAAAHGRyZWYAAAAAAAAAAQAAAAx1cmwgAAAA"
                + "AQAAAZJzdGJsAAAAunN0c2QAAAAAAAAAAQAAAKphdmMxAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAAAAKAAWgBIAAAASAAAAAAA"
                + "AAABFUxhdmM2Mi4yOC4xMDAgbGlieDI2NAAAAAAAAAAAAAAAGP//AAAAMGF2Y0MBQsAK/+EAGGdCwAraCjfkwEQAAAMABAAA"
                + "AwBgPEiagAEABWjOAlyAAAAAEHBhc3AAAAABAAAAAQAAABRidHJ0AAAAAAAADdgAAAAAAAAAGHN0dHMAAAAAAAAAAQAAABgA"
                + "AAQAAAAAFHN0c3MAAAAAAAAAAQAAAAEAAAAcc3RzYwAAAAAAAAABAAAAAQAAABgAAAABAAAAdHN0c3oAAAAAAAAAAAAAABgA"
                + "AAKQAAAACgAAAAoAAAAKAAAACgAAAAoAAAAKAAAACgAAAAoAAAAKAAAACgAAAAoAAAAKAAAACgAAAAoAAAAKAAAACgAAAAoA"
                + "AAAKAAAACgAAAAoAAAAKAAAACgAAAAoAAAAUc3RjbwAAAAAAAAABAAADtQAAAGJ1ZHRhAAAAWm1ldGEAAAAAAAAAIWhkbHIA"
                + "AAAAAAAAAG1kaXJhcHBsAAAAAAAAAAAAAAAALWlsc3QAAAAlqXRvbwAAAB1kYXRhAAAAAQAAAABMYXZmNjIuMTIuMTAwAAAA"
                + "CGZyZWUAAAN+bWRhdAAAAlQGBf//UNxF6b3m2Ui3lizYINkj7u94MjY0IC0gY29yZSAxNjUgcjMyMjIgYjM1NjA1YSAtIEgu"
                + "MjY0L01QRUctNCBBVkMgY29kZWMgLSBDb3B5bGVmdCAyMDAzLTIwMjUgLSBodHRwOi8vd3d3LnZpZGVvbGFuLm9yZy94MjY0"
                + "Lmh0bWwgLSBvcHRpb25zOiBjYWJhYz0wIHJlZj0xIGRlYmxvY2s9MDowOjAgYW5hbHlzZT0wOjAgbWU9ZGlhIHN1Ym1lPTAg"
                + "cHN5PTEgcHN5X3JkPTEuMDA6MC4wMCBtaXhlZF9yZWY9MCBtZV9yYW5nZT0xNiBjaHJvbWFfbWU9MSB0cmVsbGlzPTAgOHg4"
                + "ZGN0PTAgY3FtPTAgZGVhZHpvbmU9MjEsMTEgZmFzdF9wc2tpcD0xIGNocm9tYV9xcF9vZmZzZXQ9MCB0aHJlYWRzPTMgbG9v"
                + "a2FoZWFkX3RocmVhZHM9MSBzbGljZWRfdGhyZWFkcz0wIG5yPTAgZGVjaW1hdGU9MSBpbnRlcmxhY2VkPTAgYmx1cmF5X2Nv"
                + "bXBhdD0wIGNvbnN0cmFpbmVkX2ludHJhPTAgYmZyYW1lcz0wIHdlaWdodHA9MCBrZXlpbnQ9MjUwIGtleWludF9taW49MTIg"
                + "c2NlbmVjdXQ9MCBpbnRyYV9yZWZyZXNoPTAgcmM9Y3JmIG1idHJlZT0wIGNyZj0zNS4wIHFjb21wPTAuNjAgcXBtaW49MCBx"
                + "cG1heD02OSBxcHN0ZXA9NCBpcF9yYXRpbz0xLjQwIGFxPTAAgAAAADRliIQ6JuTk5OTk5OTk5Ouuuuuuuuuuuuuuuuuuuuuu"
                + "uuuuuuuuuuuuuuuuuuuuuuuuuuuvAAAABkGaIDqB7AAAAAZBmkA+gewAAAAGQZpgPoHsAAAABkGagD6B7AAAAAZBmqA+gewA"
                + "AAAGQZrAPoHsAAAABkGa4D6B7AAAAAZBmwA+gewAAAAGQZsgPoHsAAAABkGbQD6B7AAAAAZBm2A+gewAAAAGQZuAPoHsAAAA"
                + "BkGboD6B7AAAAAZBm8A+gewAAAAGQZvgPoHsAAAABkGaAD6B7AAAAAZBmiA+gewAAAAGQZpAPoHsAAAABkGaYD6B7AAAAAZB"
                + "moA+gewAAAAGQZqgPoHsAAAABkGawD6B7AAAAAZBmuA+gew="

        private static let demoMp3Base64 =
            ""
                + "SUQzBAAAAAAAI1RTU0UAAAAPAAADTGF2ZjYyLjEyLjEwMAAAAAAAAAAAAAAA//tAwAAAAAAAAAAAAAAAAAAAAAAASW5mbwAA"
                + "AA8AAABOAAAgjAAIDA8SEhUYHB8fIiUoKCwvMjU1ODw/P0JFSUxMT1JVVVlcX2JiZWlsbG9ydXl5fH+ChoaJjI+PkpaZnJyf"
                + "oqamqayvsrK2uby8v8PGycnMz9PT1tnc39/j5unp7O/z9vb5/P8AAAAATGF2YzYyLjI4AAAAAAAAAAAAAAAAJAQ4AAAAAAAA"
                + "IIzYmNL1AAAAAAD/+xDEAAAEdBNVVJCAMKYJrzcaIAIAAa05QAABWTo9UFAIBgkB8HwfB8oCAIBhEHwf1Ag7E4f4g3AEk/bA"
                + "YDgcDgAAAAAAKIkqmRRkCOkCSBaj94UB8BMb8CKUL6gaEvwkDSoAAADgCf/7EsQCgoUcHS+90AAgm4OldY7gTIAAAOC4IEAF"
                + "JQRgMbD4SaWFOYlBOYKASCQCLJAoAmvd3f+v1AASwAU7WEJZkYYYjCbpcmbjjCYZAgaRl30JAVOxOwf/v////03fpjDQ4w4d"
                + "MdODPv/7EMQEggUEHxgN+yJAsoOkaZ9kTIMwrxvjUM4cNOsbQwngbTXMAgZuqHV+awbJpaHP1/QAVFAYAD+JFmIIb85g0BWG"
                + "Zyu4ZkgVxg1gXnEsYxRhjmE8g5LwT/sz/7f//6oAAADAAWAA//sSxAOCBVgfJ6z7giCjA6b0bmwOAKAoJgYOY8hgYBTmPGt6"
                + "fUUZisOJEs6QAmAQMpdO91f/8h+ouiAQNsAWEOWoAYJDJtC5nljIQIKbtbdhkbl4FIZ+z/d/p+7DH59MaNowsQMMHzGT//sQ"
                + "xAOABQAfGA37IkCrg6d1zbCGwziJMJ8dI0pOtDSFHIMIwHUzVAMKcKB3cmwCzadDn6foBAATuAloiAHKdnCAcwACjTsKOYCg"
                + "4DAYJBLBQCD4rfkP6nbFK1Wf6t+70zChEw0eMWP/+xLEA4AE/B8YDfsiQH8DqLQdMBbTNYwwlB3TRv77NE4dMwhQeDIZBxRx"
                + "GnjoAsl+zwb/T9gIBLHAAwFAAgjbOEvzjdQkOyQD5LJAdgm5gP//b//8+gIAMjR4UXAAABgrSkEIyAGRYgD/+xDECYAEnB1F"
                + "4e8gcJEDZ7TNsE7Cf1kUCv0zlptozAyRROz1P94AACFwAoEYA58wA+BAI43MOIAkRwOEgligED+7f/9TP/9lP/TVCgAD+uEv"
                + "ERiMABDMpbDOeQQKANxgZ1V3NtZ6CP/7EsQOAENcHzKsd2IgeoNo9B08Fgi7/gBhBL3cWHN5XDu7aCZlvYEWc5w5fXr//pS2"
                + "pQAECBpQIAAA+YUULKIQ4ZOzxzTimKynliz+wLf8dzP//2MdDBZmhBxVpheBgGqmy8amAYRhev/7EMQbAMQ0HTun80IwjwPj"
                + "ga9oTQenEVGUPGLOmGiAwI/9QsowQLMDGDCDcxuAMF4bkzC97TL0GxME0HIYdJkgF6dbQRU0GeM/oAED3KwBCz4wxlF08o3k"
                + "8VGExkBY+tAJwYYK/Urp//sSxCGDRLgfGg37ImCJg6SNjuBMAt////bVAAQQ2AFAgAD5hRQ7RULGNLgeazAVOnynodZradFt"
                + "H/6v//RSs0SHmcBHHRmGAFAauauhqnBTGGCBqcRQZJAYc+YCMFAkN1KlMCCzARow//sQxCcAxHwdPafzIjCOg+OBr2hNI5MT"
                + "hDBKHDMpPjkyfhuzA5B2GYR5sHgnakGXNBnjP6wQCFB7hqIABuZHj8R/PZhDa6kB/nOwJNF0V3HshV6J1Tv2f9YEBFDcCASg"
                + "AVqrxJujoMP/+xLELIAEuB8aDfsiYJCDqDT9PE4LRw7qdEoEolkJPGd1tTP93//2p0AAAFQAgjI+2iSZhgEHPX0c0BQYM0NG"
                + "UNLTcBS4rf/93/0VAAJAAgArjAFLNQCwoYDJiG+n67IhWbi0DSp39P//+xDEMYBEdB1BrmHkMIcDpnCeMCb7vW7+KfX6EKh4"
                + "wBp51jBhyhBmy8kCbG4RJhxAYHTWmVTmEUAWyIRcMV6VMBCgINgU6MEiTArHOMcjooxuxxjAWB9AFAZAJlHcKLWtSnTX6iMl"
                + "S//7EsQ4AcSIGz2uYYQwjQPjQa9oTTFlAGmY+BofhyAfVB0Y+gachhgFBc0RUJkTYr/+r1oAAgiAGEAEMvEwkhIKCWYc32ba"
                + "SOgkQ2TuazZi1F3b/5H/9/pAhAHWU8iWHkMTQROhE3OdQf/7EMQ+A0SgHxoN+yJghoOkRZ7kTlAxKAL5btBAOzZ/U3////V/"
                + "rgACABRBA2AAnkSUQIUdD4wgpw3/CQFNBkj5t7Cb/0fZ9v+j7V6NwASLVSiHlA1Mx+Bg/LWo+yB4x9Ac5CgAUITR//sSxEQA"
                + "BIgfM4x3QjCKA+UJjuBOihNybFf7/v66AASFRoQHYXijAEHVMNfrcwzBzyUIIzbwNWUmB7xFc1GjM//pBRIWcj3s/gAxNwOT"
                + "hsIrODcEcxMgKD4tzOuzAPBD9HTL0UIr+r7a//sQxEqARMwdNaf3IjCMg6RVnuROAAAAgYGCBDTwsaKhyUSjADAgDKJQrAH4"
                + "c9qL5X9X2///b//9IFCCkSuUmiswfTCD8PzxYAe0AI+ziIAhQqaSUJ+UIrf9nr+9FXTVBgMQkhxiWOz/+xLETwPEgB8aDfsi"
                + "QJoD4sGvaEg6MyOZ3iEZUwAgcDPpAUY14UnlaLZr5n/9oCLBjka9EfkIJuOG4N4TBzMTABY97EzLkEPCX6SGXqoRW/ynySoI"
                + "BlT/CgUQAWlLIEmVAIZ2hgCgNAT/+xDEU4JEjB0zjHdCMJcDpHWe5EwwQfHA49M+fp3/d/9HRd/9lxjAIOfBnkH+jE2AlOG0"
                + "iU4NgEzExANPYsMs2Cr0h+Dph6bgs36PogACCBhXaAwBVmoNYwShkAYMA5XwwN+xwfDrvv/7EsRXg8QsHxwNeyJgkwPiwa9o"
                + "SN7fd0+r///LgAMAABLAA7zaqiHmgo1H7FcE8IeMgcdStVAqHZ7WFv////7er/uqAAAKAldgaAEqjr4J3jgoMJ/w0qEtwkJh"
                + "HHwYRmf5C1lXsH/t/f/7EMReAETEHUWn7SKwmoPiwa9oSH/6CAE2ggAd1tlQjTjGQDP1ukB70mM4cRShWclM8WIr/+r70fT/"
                + "//GVAAAIAlYrbAFLHnrTsGRSYe/Zu0pGAmTx8H8Zxf/Vf2f9u/06v11AgIOA//sSxGEABGQdPa5hJHCYg6WpjmBMDACRgAUJ"
                + "eZfqIxwSIL/lEC/bA6B6V81hb+i0////s6qvptUAAAECCAAAAOExkkAK9AASBhIqIHGRxgo6CABMBLtERilHx/6PkaAA7rTk"
                + "+Q5xjIIn//sQxGYCBOgdOa5lhDCYg6WpjmBM56ifYB5MYhIC0FHSEzpHf6f////SEAElICFeYTkYGIyJjnZWmOKMaYHwOBw3"
                + "muWBvDlHFo3Mvmf/0ggEOb3DWRgC1ZdZaqGxwDEH+lCSbotqPUf/+xLEaIAEzB85rmWEMJiDp/Qt6A4sIWmdOy1n5Up+37Vf"
                + "/voAAACBl4IESfFbAysECsY86wfasARKJjEGTrUeS+6f+37+r+3//6QQEVB6ABBAAoqKtOU1O7RE2LHleOgXQeK1V9jdn///"
                + "+xDEbAOEwB8rrHtiIH+DZY2OYEz0/d//HS+pIQCq0xnAwQxizJAx5MjUYcwTgcDnxNc03gDlPEn3Mt8WAgYZQAdRkjGAKTE4"
                + "aPOZw8iGBoko1sjYUODfXLd93/b//eoAAAiiRwMIAP/7EsRygAScHxwNeyJAnoOn9b08hlg+3RHcKBgYuxgdpcXDXY7jruI3"
                + "l90Lfe3r9X9H3f/rAAILooArWnxNAcAgU6u/AaaRAQvDsXJhJ6q7/u/+m52hEQCMCgBNzGoIwYxvTMM3tMv0bv/7EMR2AATY"
                + "HTGMd0IwjIOodD0wHkwbggj39N9c5iDtdCK2gT4Zd/9ACDGKAmdFHEVmF2FwaqTLhqXhYGFkAmBsYXJhU0YKKCAr/VclAQAi"
                + "IGhRKIGAIDgBYMYBDI+o4hGdpCM4aeui//sSxHqCBGAhHA17ImiMA6Xw/mBOBL++QTs+7q/p//+gAAhAYUChauJoHiypx9+A"
                + "xVJpoEAJRUEx31d3/3f//6UABgDAwBUYbmjICQtMeIbPqtLSLvd9+GvtvYNf/q///9ZhwhkBJoyx//sQxIGARQAdN6T3QHCD"
                + "A6d0zbxOzn5hph7GvRBsa4IdhhkAShG8QlwCgMZPMAJd6lyVBAITHgFoAADC24IIxAFDJtcOUUTHVI1yLv40y2Z5Bu7/eqzf"
                + "UwDYQABdqgxcU2qMQBZORdv/+xLEhoPE1B8aDfsiQI+EI4GvaExONhVMPQAIiLISNC7lzUuX//7b9n/qBAAQuFADgADiG5l9"
                + "AuFTKVXOiTUEcuNw4/7T7D5mtP2///7PqMOGMcLNGaOVJMMsRQ1tpGjWREKML4DEWxj/+xDEiwBE2B1D4e8gcIADZ7TNsFbJ"
                + "MwZ4yE0wgd3qU9+r6U3gqPMUuNf3MIoZo0E85jP6GYMJUHs4D40B40pM0y4DIG3sA4/6/pMEDMWGMwXN7AMKoP006YWTTGDz"
                + "MKECwnIICjAVM//7EsSRAsQcHTck90BwkYQjQa9oTHwwQn5tHf1KBAIIoFAjCAF2YWmFiAUQmgtqf9CXELxrvdxrb92M5/v/"
                + "/2O9v/95hwxjhpoUBxqZhgiYGr5PYarIkxhaAdk1YqjzEmjKRzDB3Gpcv//7EMSYAASoHT2jc0BwlwPlDY7gTLvXU6C4Mwho"
                + "0NEwcBKDNlmGM04SYwdQVTuqNJI0yzUiBxED2BwIDKg4wwAgAUCf1hyAk0upA0izYPJnMJvIcddo9v9KU///+vpVAAIQogAE"
                + "A4I+//sSxJwAxJgdPaNzQHCdA+NBr2hI6lAFDpo7QnFjpbwvGu9/Gtq7nHzn3//////SAAAQV3KlL/GRphaMRuN2Rt2LxhQA"
                + "qcTYVMgoViNrn9f///0KACADdC95hJG/+YMweRmXv6GY4HkY//sQxKADxPAfGg17QkCTg+OBr2RMNYIp0QGYcZpJmPFvIuCf"
                + "9hhhJjSBnE5wbxhYCrmozYSaewpphSglBEEVEGOLmbgGLBOrTHf1/RUAAhCgACMAAUAhtcgFCpoKrnAipfBB9d7+OGz/+xLE"
                + "owDE1B05rHNCMJ0EI0GvaEhOw+R///+z7jGjjJGTRsjldTDHGWNajPY1gxjDC4BdAU4QCzLnDSSzIB2nS0OfpQACCxhRRAAB"
                + "i8kALBgMM6x4+AxDdMBnEbfSHD79K+/+r/upvQb/+xDEpgAEXB8eDXsiaJGDaHQ9vBYYSY0kZhWbvMYUgtppm3YmlYLKYTIK"
                + "QC1C4hoJm/0ZoTa2g3+lBARc3woEDAECfG4XnNIswFFsERTYe/jhsTsGrMWzvq///f/9AABBdEskbAAjUf/7EsSrgER8HTmD"
                + "c2BwjgPlYY7gTpdpSkChw0hnDixpDUKiqZBKIryavsONxj+un7fusQAACIABIwABAO7CX5gYOGxcwd8OA4PU0chy2AORb2//"
                + "9H9uq0wwsxZQyy82u4wmBgTR7xJNF//7EMSxgsSkHSCs+yJgnQPjQa9oSAXkwiwWjSHLVGoecehngNraDf6vpVjmGDGjTnUz"
                + "GHCLWbHFvJsJi2mHIDod+SadeaBQaGqY0W4lOCZz6/pAAQK2ABkBGoZcJI0AiY018DlSYuyk//sSxLSAxGAdO6HzYHCbA+MB"
                + "r2hMUzknAiFLxm2hP///p7//oQAAAoAAEAIANnBd8wkKDivwOGCgDCdBRhhBwzEMuXhb/////QAAQFQDA4CMw04KRwAFJpP9"
                + "nHE6AIEScTQSLTnXf+3///sQxLmAxIwbPaZzQjCVg+NBr2RM//ubZ+kAAAiAFkIDYYZ2gEMJiU412ziYnAwoQXYeyhPcCbh+"
                + "c/9fr//v/+kEBlDgCACtSuknqIwQY6gB8jM5UycUnA1D17t///////VDigACCwL/+xLEvgAEjB1DoL9gMJ2DpzXNsIYYBgcv"
                + "JgBzAAINPwA00CBIBoKMMJYKEi6/7Pu////+kABAgaUWshCzop7AwRmb+WepCispS60OuU61l/k9uz9PjP88YuChhnyZzohh"
                + "piBmvBDCa3r/+xDEwgLEiB03o3NgcJoD40GvZEgg5hqAqHTbmdUmSSGRnmBEu5Tgmc+r6DAhjAEzEqjOaTBhFyMxi9YzAxaj"
                + "BSBbBqAsCaoBwOhDLfWTX/6KAEgAS0RgDnzAD4EADjcQ4gARHTAawf/7EsTGAAUIHxgNe0JAlgOm8c2wjixQEl//6ne7V2f7"
                + "Lv0hAROfgUAUABQzcUZkdTFa0DphpMloT8tebayas0f1f//0VQACADAQHiRl6ChhcMnLrycnDIOFiC7DyUBjp+iv//3////o"
                + "AP/7EMTJAASMHTODceDwkoNm8c2whgIAolAEDAD5hRQsohDhkrRHHOKYvVIYs/s73b8Vb///HIac/+gAAACURMJAAOKFjEAG"
                + "zEwkPL848YJAETwBgQgV8CYPw/LkSyuFasj6P6/XrsMI//sSxM4AhOAdM4TxgTCMA+g1zLCOETByAwxDMhmDBoH/M0//8zNh"
                + "9TBVCIIJys86AT1gB3TFZ6939X0qAAABAmddAAEKZohWcAGJIenNlBnLofmJAEHEZasvII4sDqC38PN9/v/ZZTcA//sQxNMA"
                + "RHAdO4ZxgrCNg6c0nmgOTAFAkDAD5hRQ7RULGMLod6zAVHHseZlrNbTv6f//s07f/1UAAAAUAiIAAM1QjBzh4pmEcDcaGZ9x"
                + "oMA4GEeBCdyxjIAVEGYF9JWEe//v/3bPsBD/+xLE2QPFAB8aDXtCQJYD44GvZEgwg0oADDVVhQIh4YAgR5hjp9ngRYBGFU2h"
                + "qRQHL+nTX93r//b9xRUAAAAcZRxoANsqAOWfhGNIinnlbnlIjmNAHH1YBOFDCH6e9I30dzdf3/tppn7/+xDE3AAEbB09Rm2C"
                + "sIcDqPQ94A7f+kwEKAA2ATowqHMDscoyJuajIVHCMCYHsKSCTgaMdhYla0KdNfrVAAAKoAcACABkfbRJMwwBjn7mObAYOGaG"
                + "jKGlpuAoFmlzP/kvJf///3pMOP/7EMTigARgHTMk8eDwmAOndP5oRnTCTQwhlMZ1jAVArkwtVV9MK8CrTANwMYxMkDTweFOm"
                + "WDMSeMivd/X8kgAAAIAWEgRZ7F/iRDFILPUQc8+DQgoBA1M14EI3jwzP/3dT///v/1gC//sSxOeDRZAbL6fx4nCfhGMBv2RI"
                + "uAAO0zFN4ciFxoMWvkOvmFBqwDqMeV65t/nKCLSPjP///rpqAAAgIC9JWPDJh2Rph0AvGzabUbIgMxhygTnSUGSTGBUgywKD"
                + "IncFup1H0f//TQGA//sQxOcABYQdKax3AmCOA6eo/mRGgAnKKgDCIj4AAc5hPvMnMcYhNAuEojkwSLCyLUmNf9H//b//9NUA"
                + "AAoAG0AFrJYjy4SgY/g+flMifYhKY+AQcpYBLEBgzSm/MC38X///o7Yp/8b/+xLE6IIF2B0hrPsiYJ4DpXWPbEQgQYOEvlD4"
                + "guKg+GBwj4bX0SFUjGSKcpwtRozP/6v/2gAAApXFIkAAqUsVgNcMIB3cwBLYEiGB8HwTMHAkMrmoxO3ZV6FLoGO/uSBOso3E"
                + "AE5RkAf/+xDE5wPF6B0lrHcCYJQD40G/ZEwRIehQOcwQXozPOUcNgSFoVlAYRCyMUmNWVp2UavV/Z0X22f0qMPAA5JJpMFkB"
                + "izgTnM8MicsIHJiwAVHBoJkaKYEkCOTISF6KG5sh8v9P/1AuAP/7EsTmAkUoGzOk8YEwrYRigb/oSMhAcJfKHYqXLAPwWSAE"
                + "PwZKoYMEVrUQatRmbb//6/+tFv/6ajHgsDOQtmkcWLHaHkGECeGgChjogZnYKxmTAYO0jGMSmTVZJdqE0+n6f//pv/+kwP/7"
                + "EMTlgkT0HS+McwJwkoPl9Y7oRoTCpUOKoxeAEJrMByTJzAdQmMwBwC0NPLNSPDOguPKJ6jEnvGv3ff/9tQMODFoliE6JiHgI"
                + "G7EMMAuajEIAKO6lMitCzAk1kBOCror+r6QIBoIU//sSxOiDBbQfGw17QlCeA6SNn2xEJlVgK9zAEGYMKHOowoBlDAVBxNSo"
                + "0ghL8TMJoGz3jX6///rVAAAAlVFBQACPosELQh8xhKAJmiuLiaHQBYYJGdRAEOHUSHJD6ZFb9Gyj8WUv/9yQ//sQxOeCRaQd"
                + "I0z3ImB/A6Wdj2hERXEAoZbEomQKFRbMLNvNXZEAhOdl7QF/vfeNdfJH0a//0gAAABQBSAF2g40OyBfJhWgWGngR4aaIEgkK"
                + "YeZYBTIFhj8cDmhb2f//7MZ9ALrzyQH/+xLE6oIFTB0xp/HicLqDpLWfbET2aKi4O3CgPhgto7HY9AUqXUVwqNMxqFGZvtZm"
                + "v/f+z/zv6DGg8zEINWDTqSYxmArzrdYvOpgJsaMbOONjHEwdVQDUAkoaTNVWIf7vu//+9IADxkr/+xDE6AIFdCMUDftiQJaD"
                + "pbGPaEYEKqYZdGAihMZhHyXaYRmEumAsAWxuaZrTJvARxx4lPVokdg1+/7oxUKMmBzSQw44pMWELQ5emYDk9CkDirTdykxM9"
                + "JUYwCRBA+4Uzc3f5X6AMAP/7EsTog8XYIxAN+2JAqoQiwb/oSIAHSYyQAEfTAACQMKFQw5yOMHHQQAJgJdoUMUo3Q9//////"
                + "/dUAAAHBVWNoAKao5BzQH4xoDk9BoE8sDMaMoHsEAiQor5PaqK0fJ0avU7V0/cn/0P/7EMTlg0SYHxgNe0JAngPjia9kTAAE"
                + "J3iyiQACrQQWwgZDJiS9nKyrYLedaoVZTujVlZDV6vu/TJdvb7UIAAUGRxDHH4ZjSIR51Zx5KHgcZIPWIQkJBF9PWmb//9H/"
                + "//31hgGBeYqk//sSxOiCRfAdIaz7ImCJg6Y1juhGQosMXxMEAYwyL8gjIiGIME0HE5sDWMAX5yGCUDn2DQ///2IyYdM7HDZC"
                + "071kMcYWc8C7rTu8FTMbcCo5gqMUUAAwmN3xhpcsLEqrL/f6n//R//9A//sQxOmCBYwdH4z7ImCeg6Wxj2hGEAERoEBhsDJu"
                + "6OiH5hlmAtuww933Ucdv7xrirNbf+5G/bQAAARFvCgMSVyXqOATEcOjmqlDlsNgwiRKae6OwiiwGsLcdxTbvQ5b/7f9H75Co"
                + "AeD/+xLE6QPFpCESDftiQKqEIsG/6EgEAFAAgJo6OgXqYBAOxhsohHpOhhoqWUUETXRQZpbdbs/3///oAAABkWYAAzldSGoC"
                + "sYdBucKzecDBmGDcNEWAUNFVMqxFYF07KP6Xf///pEQKKDT/+xDE5wKFOCMUDftiQJqDpXWPbERgqCZNLGD2PyZ1H35nKj6m"
                + "EQEwcOWak+bAgbdOCm6/J+wHv2oxkSMmFzRSg4RhMUsVo48rQji5FMMTwDk2UJAJiYOgGNQ5hg6xKU1hZPznrIlsKv/7EsTo"
                + "AAXcHSWsdwJgn4Ontcy8hjQAqM3rMGAXoy/b9DLqF2MGkG07sjYSN8c4mghxv7A5ugAAApR3sgNdYkmCAlGFgbm4c2m2wahA"
                + "rJMMwWBFSNaOnYCmOO8l7erLu/vX6e9oABBF0v/7EMTmgkSkHSRMdwJwogPjpa9kTLEbYANEPGnoDRKZq6J6UaARAOu9+HLd"
                + "e8+I624+ju+j6P9H5ykfAAAB3YVjjACpkiQUI6PMTxmOnvYOjRgMSgDHpqLl9gpVTGmCV13b93V/b93X//sSxOkARhQhEA37"
                + "YkCIA6j8HeAO/SAAQRQOAOFugnuDBEZz5B7kReNAIux+3IdS3yX/////u/+hMgHTLyQ0xBOVjDFqHfOb/yc5fhzTFZBiB2gF"
                + "jQxhHMvlDGyBTWNVT37fu6/6f//o//sQxOmCBdgdKYx3AnCdA6W9j2xESVCxAxbE1nYwiRnTQCz9M+sZswkgfDgvzQnTSEjT"
                + "rQMjbS2GXfUqMyWNIfN+4P1/MTUbQ4Yt6jgrGmMSQFsbVjKcyrg1H0yhldUZBn8v9n//0iH/+xLE6ALFWB0rjHcCcJ+EIwG/"
                + "aEyX0ATTpqUA1ZgkLBofshwgsGFQaWkUEU3RMcS2Z/u9Xi/TyH3IMoKTNzQ1hXOltjBjglsyrFLbMpqCRzBeQIgo4SA4MoTz"
                + "Q6QycmSth6lPfv9Xr/r/+xDE6ILFeCEUDftiQI8D45WvZE8TAdt212or07D4C0YlKA+H0+2BmWSZYK65Q7DkQ5NhqBMEIgp9"
                + "D1fHgK9ns8eazf3V7+Ph4wRIacVkTw37+9H92hEgAD0YDYbDAUCAAAAAAPlAeP/7EsTqAAXUGy2MdwJwqQOnNG5oDi8NVK8D"
                + "0oANUrOmGABoNjnhd4WFB/TFFuICDhFBDOpJP8jSEIkOUTf/mhdMjNzX9REAiqpMQU1FMy4xMDCqqqqqqqqqqqqqqqqqqqqq"
                + "qqqqqqqqqv/7EMTngIXMHSesdwJgiYPnMG5oDqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq"
                + "qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq//sSxOiDxgAhEg37YkCYg+NBr2hMqqqqqqqqqqqqqqqq"
                + "qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq"
                + "//sQxOcCBVwfFA17QkCYg6XljvBGqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq"
                + "qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqr/+xLE6ABF6CEQDf9iQQaOKray8Aeqqqqqqqqqqqqqqqqqqqqqqqqq"
                + "qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqr/+xDE2YAI"
                + "hIdduUkAEAAANIOAAASqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq"
                + "qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqg=="

        /// Serve the demo video's poster with no gateway: a flat 1280×720 PNG
        /// plus a fake duration, so the downloaded tile (poster + play disc +
        /// duration chip) is screenshot-verifiable headlessly.
        func serveDemoVideoPosterIfRequested(id: Int, blobId: String) -> Bool {
            guard blobId == Self.demoVideoAttachment["blob_id"] as? String else { return false }
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(600))
                guard let png = Self.demoImagePng(width: 1280, height: 720) else { return }
                pushDemoVideoPoster(
                    id: id, dataBase64: png.base64EncodedString(),
                    width: 1280, height: 720, durationMs: 83_000)
            }
            return true
        }

        /// `-baybo-demo-attachments` (DEBUG): one short agent turn carrying the
        /// file chips above plus the audio card and video tile, so the
        /// attachment styling fits on a single screenshot instead of scrolling
        /// off above a long markdown answer.
        func startDemoAttachmentsIfRequested() {
            guard ProcessInfo.processInfo.arguments.contains(Self.demoAttachmentsArg) else {
                return
            }
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(500))
                pushDemoUserSent(
                    msgId: "demo-att-user", text: "把角色卡和架构评审发我",
                    attachments: [Self.demoSentAttachment])
                pushDemo(["kind": "turn_state", "active": true])
                try? await Task.sleep(for: .milliseconds(600))
                pushDemo([
                    "kind": "message", "role": "assistant",
                    "content": "已生成好了，附件里有角色卡和这一季的架构评审。",
                    "platform_msg_id": "demo-att-1", "ordinal": 1,
                    "attachments": Self.demoFileAttachments
                        + [Self.demoAudioAttachment, Self.demoVideoAttachment],
                ])
                pushDemo(["kind": "turn_state", "active": false])
                // The same card renders right-aligned on a user send, under the
                // outbox's sending chrome — verify both, not just the agent side.
                try? await Task.sleep(for: .milliseconds(400))
                pushDemo([
                    "kind": "message", "role": "user", "content": "这份也帮我看下",
                    "platform_msg_id": "demo-att-u2", "ordinal": 2,
                    "attachments": [Self.demoFileAttachments[0]],
                ])
                await driveDemoDownloadIfRequested()
            }
        }
        @MainActor private static var demoSwitchSeeded = Set<String>()

        func startDemoFramesIfRequested() {
            guard ProcessInfo.processInfo.arguments.contains(Self.demoFramesArg) else { return }
            NSLog("baybo: demo frames starting")
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(500))
                pushDemoUserSent(msgId: "demo-user-1", text: "Give me an overview of this project")
                pushDemo(["kind": "turn_state", "active": true])
                try? await Task.sleep(for: .milliseconds(700))
                for chunk in ["Let me look at the repo structure first.", "The core is a multi-channel assistant framework,", "let me read the module docs before summarizing…"] {
                    pushDemo(["kind": "reasoning", "text": chunk])
                    try? await Task.sleep(for: .milliseconds(400))
                }
                pushDemo([
                    "kind": "tool_started", "call_id": "demo-t1", "tool": "read_file",
                    "label": "Read docs/modules/README.md",
                ])
                try? await Task.sleep(for: .milliseconds(1200))
                pushDemo([
                    "kind": "tool_completed", "call_id": "demo-t1", "status": "ok",
                    "summary": "31 modules",
                ])
                pushDemo(["kind": "reasoning", "text": "Docs are thorough — I can summarize directly."])
                try? await Task.sleep(for: .milliseconds(600))

                let answer = """
                    **Baybo** is an LLM-powered assistant framework with multi-channel access and tool calling.

                    ## Core Capabilities

                    - Multi-channel access (Telegram, Discord, Web)
                    - Tool calling and `Skill` extensions
                    - Context management, compression, and error recovery

                    ```rust
                    let client = BayboClient::new(config)?;
                    client.chat_send(session_id, "hello").await?;
                    ```

                    | Module | Responsibility | Key Deps | Status | Owner |
                    | --- | --- | --- | --- | --- |
                    | gateway | Channel gateway & session routing | tokio, axum | stable | core-team |
                    | wire | Frame protocol & serialization | serde, rmp-serde | stable | core-team |
                    | agent | Agent execution loop | tokio, baybo-tools | iterating | agent-team |
                    | Dicklesworthstone/destructive_llm_agent_experiments | 一个比列宽还长、且没有断行机会的 token —— 必须在格子内折行,不能压到隔壁列上 | serde | stable | ext |

                    A narrow table stretches to fill the row:

                    | Metric | Value |
                    | --- | --- |
                    | Modules | 31 |
                    | Coverage | 87% |

                    See the [module index](https://example.com/docs).

                    ## Math rendering

                    Inline math like \\(E = mc^2\\) and $a^2 + b^2 = c^2$ sit in the
                    prose, while a display block stands on its own:

                    $$\\int_0^\\infty e^{-x^2}\\,dx = \\frac{\\sqrt{\\pi}}{2}$$

                    A narrow equation centers; a very wide one scrolls in place (left edge reachable) instead of the page:

                    $$e^{i\\pi} + 1 = 0$$

                    $$\\hat{H}\\psi = \\left(-\\frac{\\hbar^2}{2m}\\nabla^2 + V(\\mathbf{r})\\right)\\psi = \\sum_{n=0}^{\\infty} c_n \\phi_n e^{-iE_n t/\\hbar} = i\\hbar\\frac{\\partial \\psi}{\\partial t} = E\\psi \\quad\\text{for all}\\quad \\psi \\in \\mathcal{H}$$

                    Numbered steps keep their structure around display math:

                    1. Start from \\(a^2 + b^2 = c^2\\).
                    2. The closed form is $$c = \\sqrt{a^2 + b^2}$$
                    3. Done.

                    Arithmetic like $2 + 2 = 4$ and a decimal $3.14$ still render as
                    math, while prose money stays literal — it costs $5 today, was
                    $12.50 last week. A dollar sign in `code like $PATH` stays literal too.
                    """
                for chunk in Self.chunked(answer, size: 14) {
                    pushDemo(["kind": "answer_delta", "text": chunk])
                    try? await Task.sleep(for: .milliseconds(90))
                }
                try? await Task.sleep(for: .milliseconds(500))
                pushDemo([
                    "kind": "message", "role": "assistant", "content": answer,
                    "platform_msg_id": "demo-a-1", "ordinal": 1,
                ])
                pushDemo(["kind": "turn_state", "active": false])
            }
        }

        /// `-baybo-demo-approval`: a turn that blocks on the approval gate, so the
        /// native card AND the work block's awaiting/result labels are verifiable
        /// with no gateway. Frames ride the real `pushFrame` path, so the native
        /// observer and the web reducer are the production ones; only the leg is
        /// fake — `resolveDemoApprovalIfRequested` answers the card in-process
        /// (there is no `chatResolveApproval` to call without a binding) by
        /// pushing exactly the frames the gateway would: `approval_resolved`, then
        /// the tool's completion carrying the decision.
        ///
        /// Two prompts open at once (a parallel tool pair) so the queue counter
        /// renders; the second is left for the tester to answer.
        func startDemoApprovalIfRequested() {
            guard ProcessInfo.processInfo.arguments.contains("-baybo-demo-approval") else { return }
            NSLog("baybo: demo approval starting")
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(500))
                pushDemoUserSent(msgId: "demo-appr-u1", text: "Clean up the build artifacts")
                pushDemo(["kind": "turn_state", "active": true])
                try? await Task.sleep(for: .milliseconds(600))
                pushDemo(["kind": "reasoning", "text": "That deletes files — I'll ask first."])
                try? await Task.sleep(for: .milliseconds(400))

                for (call, label, command) in [
                    ("demo-call-1", "rm -rf target/debug", "rm -rf target/debug"),
                    ("demo-call-2", "rm -rf build/", "rm -rf build/"),
                ] {
                    pushDemo([
                        "kind": "tool_started", "call_id": call, "tool": "Bash", "label": label,
                    ])
                    pushDemo([
                        "kind": "approval_requested",
                        "call_id": "demo-prompt-\(call)",
                        "tool_call_id": call,
                        "session_id": sessionId,
                        "tool": "Bash",
                        "accesses": [["kind": "exec_command", "command": command]],
                        "params_preview": "{\"command\": \"\(command)\"}",
                        "description": "Delete stale build output",
                    ])
                    try? await Task.sleep(for: .milliseconds(300))
                }
            }
        }

        /// Answer a demo approval locally: no binding exists, so the FFI call
        /// would fail. Pushes the frames the gateway would have — the resolution
        /// broadcast, then the blocked call's completion carrying the decision
        /// (a deny surfaces as `status: denied`, exactly as the executor's
        /// `ToolError::Denied` does).
        func resolveDemoApprovalIfRequested(
            _ approval: PendingApproval, decision: ApprovalDecision
        ) -> Bool {
            guard ProcessInfo.processInfo.arguments.contains("-baybo-demo-approval"),
                approval.callId.hasPrefix("demo-prompt-"),
                let toolCallId = approval.toolCallId
            else { return false }
            let decisionText =
                switch decision {
                case .approve: "approve"
                case .deny: "deny"
                }
            Task { @MainActor in
                pushDemo([
                    "kind": "approval_resolved", "call_id": approval.callId,
                    "decision": decisionText,
                ])
                try? await Task.sleep(for: .milliseconds(decision == .deny ? 300 : 1400))
                pushDemo([
                    "kind": "tool_completed", "call_id": toolCallId,
                    "status": decision == .deny ? "denied" : "ok",
                    "summary": decision == .deny ? "denied" : "removed 1.2 GB",
                    "approval": decisionText,
                ])
            }
            return true
        }

        /// `-baybo-demo-html` (DEBUG, with `-baybo-open-chat`): one agent turn
        /// whose answer carries a `baybo-html` marker, so the inline preview
        /// card, its fullscreen expansion and the left-edge swipe that leaves
        /// that fullscreen (rather than the conversation) are all drivable with
        /// no gateway. The bytes behind the marker are served by
        /// `TranscriptSchemeHandler` under the same flag — a demo session has no
        /// leg, so a real blob read could only ever answer `notFound`.
        func startDemoHtmlIfRequested() {
            guard ProcessInfo.processInfo.arguments.contains(Self.demoHtmlArg) else { return }
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(400))
                pushDemoUserSent(msgId: "demo-html-u1", text: "Make me a small sales dashboard")
                pushDemo(["kind": "turn_state", "active": true])
                try? await Task.sleep(for: .milliseconds(500))
                let answer = #"""
                    Here it is — tap the expand control for the full screen.

                    ```baybo-html
                    \#(TranscriptSchemeHandler.demoHtmlBlobId)
                    ```

                    Want it broken down by month as well?
                    """#
                pushDemo([
                    "kind": "message", "role": "assistant", "content": answer,
                    "platform_msg_id": "demo-html-1", "ordinal": 1,
                ])
                pushDemo(["kind": "turn_state", "active": false])
            }
        }

        func startDemoSwitchIfRequested() {
            guard ProcessInfo.processInfo.arguments.contains("-baybo-demo-switch") else { return }
            guard Self.demoSwitchSeeded.insert(sessionId).inserted else { return }
            let tag = sessionId.uppercased()
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(350))
                pushDemoUserSent(
                    msgId: "u-\(sessionId)",
                    text: "Conversation \(tag): only \(tag) content belongs in this thread.")
                pushDemo(["kind": "turn_state", "active": true])
                try? await Task.sleep(for: .milliseconds(250))
                let answer =
                    "Reply for **\(tag)**. Seeing \(tag) alone (no other session's text) proves the reused webview kept sessions isolated across the retarget."
                for chunk in Self.chunked(answer, size: 12) {
                    pushDemo(["kind": "answer_delta", "text": chunk])
                    try? await Task.sleep(for: .milliseconds(50))
                }
                pushDemo([
                    "kind": "message", "role": "assistant", "content": answer,
                    "platform_msg_id": "m-\(sessionId)", "ordinal": 1,
                ])
                pushDemo(["kind": "turn_state", "active": false])
            }
        }

        private func pushDemo(_ object: [String: Any]) {
            guard let data = try? JSONSerialization.data(withJSONObject: object),
                let json = String(data: data, encoding: .utf8)
            else { return }
            pushDemoFrame(json)
        }

        private static func chunked(_ text: String, size: Int) -> [String] {
            var out: [String] = []
            var index = text.startIndex
            while index < text.endIndex {
                let end = text.index(index, offsetBy: size, limitedBy: text.endIndex) ?? text.endIndex
                out.append(String(text[index..<end]))
                index = end
            }
            return out
        }
    }

    /// `-baybo-demo-compose` (DEBUG, with `-baybo-open-chat`): seed the
    /// composer's staged strip with one of each state — a ready image, a ready
    /// file, one mid-upload (byte counter) and one failed (retry affordance) —
    /// so the strip is screenshot- and XCUITest-verifiable headlessly. The
    /// system document picker is out-of-process UI a UI test can barely reach,
    /// so staging by hand is not a path a smoke can take.
    ///
    /// Every name here is prefixed `staged-`: this flag runs ALONGSIDE
    /// `-baybo-demo-attachments` (a strip surviving what the transcript
    /// presents is exactly what one smoke drives), and a by-label query that
    /// matches both a strip tile and a transcript card picks whichever it
    /// pleases.
    extension StagedAttachment {
        private static let demoComposeArg = "-baybo-demo-compose"
        /// Nothing retries or uploads under this flag (there is no gateway);
        /// the source only has to exist. `.scoped` on purpose — it is the case
        /// nothing ever deletes, so dismissing a demo tile can't try to unlink
        /// this path.
        private static let demoSourceUrl = URL(fileURLWithPath: "/dev/null")

        static func demoStagedIfRequested() -> [StagedAttachment] {
            guard ProcessInfo.processInfo.arguments.contains(demoComposeArg) else { return [] }
            return [
                StagedAttachment(
                    preview: .image(demoThumbnail()), source: .scoped(demoSourceUrl),
                    mime: "image/jpeg", filename: nil, byteCount: 1_482_752,
                    state: .ready(blobId: "sha256:democompose1.tok")),
                demoFile(
                    name: "staged-architecture-review-2026-Q3.pdf",
                    mime: "application/pdf", byteCount: 2_413_512,
                    state: .ready(blobId: "sha256:democompose2.tok")),
                demoFile(
                    name: "staged-landing-flow-capture.mp4", mime: "video/mp4",
                    byteCount: 24_804_352, state: .uploading(sent: 9_120_000)),
                demoFile(
                    name: "staged-quarterly.numbers", mime: StagedAttachment.fallbackMime,
                    byteCount: 812, state: .error),
            ]
        }

        private static func demoFile(
            name: String, mime: String, byteCount: UInt32, state: State
        ) -> StagedAttachment {
            StagedAttachment(
                preview: .file(name: name, mime: mime), source: .scoped(demoSourceUrl),
                mime: mime, filename: name, byteCount: byteCount, state: state)
        }

        private static func demoThumbnail() -> UIImage {
            let size = CGSize(width: 240, height: 240)
            let format = UIGraphicsImageRendererFormat.default()
            format.scale = 1
            return UIGraphicsImageRenderer(size: size, format: format).image { ctx in
                UIColor(white: 0.85, alpha: 1).setFill()
                ctx.fill(CGRect(origin: .zero, size: size))
            }
        }
    }
#endif
