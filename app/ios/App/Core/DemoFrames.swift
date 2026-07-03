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
                bridge?.userSent(msgId: "demo-user-1", text: "介绍一下这个项目", attachments: [])
                pushDemo(["kind": "turn_state", "active": true])
                try? await Task.sleep(for: .milliseconds(700))
                for chunk in ["让我先看看仓库结构。", "核心是一个多通道的智能助手框架,", "读一下模块文档再总结…"] {
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
                pushDemo(["kind": "reasoning", "text": "文档齐全,可以直接总结了。"])
                try? await Task.sleep(for: .milliseconds(600))

                let answer = """
                    **Baybo** 是一个基于大语言模型的智能助手框架,支持多通道接入与工具调用。

                    ## 核心能力

                    - 多通道接入(Telegram、Discord、Web)
                    - 工具调用与 `Skill` 扩展
                    - 上下文管理、压缩与错误恢复

                    ```rust
                    let client = BayboClient::new(config)?;
                    client.chat_send("hello").await?;
                    ```

                    | 模块 | 职责 |
                    | --- | --- |
                    | gateway | 通道网关 |
                    | wire | 帧协议 |

                    详见[模块索引](https://example.com/docs)。
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
