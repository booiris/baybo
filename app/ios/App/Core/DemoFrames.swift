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
            // Register the session the way a real send would: an unregistered
            // session is not "mirror-worthy", so `TranscriptStore.prune` deletes
            // its mirror on the next list sync — and the second run, the one that
            // has to restore the recorded image sizes, would open with none.
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

        /// Stand-in bytes for a demo blob a share/preview wants materialised —
        /// enough for the system share sheet (and QuickLook, for the PDF) to
        /// present headlessly; the media ones are NOT playable.
        static func demoMaterializeBytes(blobId: String) -> Data? {
            let pdfId = demoFileAttachments[0]["blob_id"] as? String
            let audioId = demoAudioAttachment["blob_id"] as? String
            let videoId = demoVideoAttachment["blob_id"] as? String
            switch blobId {
            case pdfId:
                return Data("%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n".utf8)
            case audioId, videoId:
                return Data(count: 4096)
            default:
                return nil
            }
        }

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
                pushDemoUserSent(msgId: "demo-att-user", text: "把角色卡和架构评审发我")
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

                    A narrow table stretches to fill the row:

                    | Metric | Value |
                    | --- | --- |
                    | Modules | 31 |
                    | Coverage | 87% |

                    See the [module index](https://example.com/docs).
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
#endif
