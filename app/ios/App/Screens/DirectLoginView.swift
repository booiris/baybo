import SwiftUI

/// Direct (URL + admin token) login form. URL normalization feedback mirrors
/// the Rust side (default https://, strip trailing slash) for instant local
/// validation; the stable `invalidToken` error variant maps to a localized
/// message; the token never outlives the attempt.
struct DirectLoginView: View {
    @EnvironmentObject private var store: AppStore
    @State private var url = ""
    @State private var token = ""
    @State private var error: String?
    @State private var connecting = false
    @FocusState private var focus: Field?

    private enum Field {
        case url
        case token
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Button {
                    store.landingView = .menu
                } label: {
                    Label("direct.back", systemImage: "chevron.left")
                        .font(Theme.mono(13))
                        .foregroundStyle(Theme.ink)
                }
                Spacer()
            }
            .padding(.top, 8)

            Spacer()

            VStack(alignment: .leading, spacing: 18) {
                Text("direct.hint")
                    .font(Theme.mono(13))
                    .foregroundStyle(Theme.inkSoft)

                VStack(alignment: .leading, spacing: 6) {
                    Text("direct.urlLabel")
                        .font(Theme.mono(12))
                        .foregroundStyle(Theme.inkSoft)
                    TextField("baybo.example.com", text: $url)
                        .textContentType(.URL)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .focused($focus, equals: .url)
                        .submitLabel(.next)
                        .onSubmit { focus = .token }
                        .fieldChrome()
                }

                VStack(alignment: .leading, spacing: 6) {
                    Text("direct.tokenLabel")
                        .font(Theme.mono(12))
                        .foregroundStyle(Theme.inkSoft)
                    SecureField("direct.tokenPlaceholder", text: $token)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .focused($focus, equals: .token)
                        .submitLabel(.go)
                        .onSubmit { connect() }
                        .fieldChrome()
                    Text("direct.tokenHint")
                        .font(Theme.mono(11))
                        .foregroundStyle(Theme.inkSoft.opacity(0.8))
                }

                if let error {
                    Text(verbatim: error)
                        .font(Theme.mono(12))
                        .foregroundStyle(Theme.err)
                }
            }

            Spacer()

            Button {
                Haptics.tap()
                connect()
            } label: {
                Text(connecting ? "direct.connecting" : "direct.connect")
            }
            .buttonStyle(InkPillButtonStyle())
            .disabled(connecting || normalizedUrl == nil || token.trimmed.isEmpty)
            // The web `button:disabled` dim.
            .opacity(normalizedUrl == nil || token.trimmed.isEmpty ? 0.35 : 1)
        }
        .padding(.horizontal, 28)
        .padding(.bottom, 18)
    }

    /// Local mirror of the Rust `normalize_base` for instant feedback; the core
    /// re-validates authoritatively.
    private var normalizedUrl: String? {
        let trimmed = url.trimmed
        guard !trimmed.isEmpty else { return nil }
        let withScheme = trimmed.contains("://") ? trimmed : "https://\(trimmed)"
        let lowered: String
        if let idx = withScheme.range(of: "://") {
            lowered =
                withScheme[..<idx.lowerBound].lowercased()
                + String(withScheme[idx.lowerBound...])
        } else {
            lowered = withScheme
        }
        guard lowered.hasPrefix("https://") || lowered.hasPrefix("http://") else { return nil }
        return String(lowered.reversed().drop(while: { $0 == "/" }).reversed())
    }

    private func connect() {
        guard let base = normalizedUrl, !token.trimmed.isEmpty, !connecting else {
            if normalizedUrl == nil {
                error = String(localized: "direct.invalidUrl")
            }
            return
        }
        connecting = true
        error = nil
        let attemptToken = token.trimmed
        Task {
            let failure = await store.directConnect(baseUrl: base, token: attemptToken)
            connecting = false
            error = failure
            if failure == nil {
                token = "" // never keep the admin token in view state
            }
        }
    }
}

extension String {
    fileprivate var trimmed: String {
        trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

extension View {
    fileprivate func fieldChrome() -> some View {
        font(Theme.mono(14))
            .foregroundStyle(Theme.ink)
            .padding(.horizontal, 14)
            .padding(.vertical, 12)
            .background(
                RoundedRectangle(cornerRadius: Theme.radius)
                    .fill(Theme.surface)
            )
            .overlay(
                RoundedRectangle(cornerRadius: Theme.radius)
                    .strokeBorder(Theme.line, lineWidth: 1)
            )
    }
}
