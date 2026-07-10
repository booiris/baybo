#if DEBUG
    import Foundation

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

        /// `-baybo-demo-download` (with `-baybo-demo-attachments`): push the
        /// `fileState` messages a real download would, so the card's idle →
        /// loading (spinner + byte counter) → ready transition is
        /// screenshot-verifiable with no gateway and no blob leg. It drives the
        /// exact web reducer the native path drives; only the bytes are fake.
        private func driveDemoDownloadIfRequested() async {
            guard ProcessInfo.processInfo.arguments.contains(Self.demoDownloadArg) else { return }
            let blobId = Self.demoFileAttachments[0]["blob_id"] as? String ?? ""
            let total = Self.demoFileAttachments[0]["size"] as? Int ?? 0
            try? await Task.sleep(for: .milliseconds(1500))
            for step in 1...6 {
                let loaded = UInt64(total * step / 8)
                pushDemoFileState(
                    blobId: blobId, state: "loading", loaded: loaded, total: UInt64(total))
                try? await Task.sleep(for: .milliseconds(700))
            }
            pushDemoFileState(blobId: blobId, state: "ready")
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

        /// `-baybo-demo-attachments` (DEBUG): one short agent turn carrying the
        /// file chips above, so the attachment styling fits on a single
        /// screenshot instead of scrolling off above a long markdown answer.
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
                    "attachments": Self.demoFileAttachments,
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
