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

        func startDemoFramesIfRequested() {
            guard ProcessInfo.processInfo.arguments.contains(Self.demoFramesArg) else { return }
            NSLog("baybo: demo frames starting")
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(500))
                bridge?.userSent(msgId: "demo-user-1", text: "Give me an overview of this project", attachments: [])
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
                    client.chat_send("hello").await?;
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

        private func pushDemo(_ object: [String: Any]) {
            guard let data = try? JSONSerialization.data(withJSONObject: object),
                let json = String(data: data, encoding: .utf8)
            else { return }
            bridge?.pushFrame(json)
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
