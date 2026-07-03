import SwiftUI

/// The direct (URL + admin token) login form — the web `.direct-form`, rendered
/// INSIDE the landing hero (the wordmark + rule stay above it). Left-aligned
/// labeled fields, a full-width connect CTA, a quiet "← Back" link, and the
/// status line. URL normalization mirrors the Rust side for instant feedback;
/// the token never outlives the attempt.
struct DirectLoginForm: View {
    @EnvironmentObject private var store: AppStore
    @ObservedObject private var lang = Lang.shared
    @State private var url = ""
    @State private var token = ""
    @State private var status: String?
    @State private var connecting = false
    @FocusState private var focus: Field?

    private enum Field {
        case url
        case token
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(verbatim: lang.t("direct.hint"))
                .font(Theme.mono(15))
                .foregroundStyle(Theme.inkSoft)
                .lineSpacing(6)

            // .field: label over input, 0.3rem gap.
            VStack(alignment: .leading, spacing: 5) {
                fieldLabel(lang.t("direct.urlLabel"))
                TextField("https://…", text: $url)
                    .textContentType(.URL)
                    .keyboardType(.URL)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .focused($focus, equals: .url)
                    .submitLabel(.next)
                    .onSubmit { focus = .token }
                    .fieldChrome()
            }

            VStack(alignment: .leading, spacing: 5) {
                fieldLabel(lang.t("direct.tokenLabel"))
                SecureField(lang.t("direct.tokenPlaceholder"), text: $token)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .focused($focus, equals: .token)
                    .submitLabel(.go)
                    .onSubmit { connect() }
                    .fieldChrome()
                Text(verbatim: lang.t("direct.tokenHint"))
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.inkSoft)
            }

            // .direct-form .cta is full width (max-width: none).
            Button {
                Haptics.tap()
                connect()
            } label: {
                Text(verbatim: lang.t(connecting ? "direct.connecting" : "direct.connect"))
            }
            .buttonStyle(InkPillButtonStyle())
            .disabled(connecting || url.trimmed.isEmpty || token.trimmed.isEmpty)
            // The web `button:disabled` dim.
            .opacity(url.trimmed.isEmpty || token.trimmed.isEmpty ? 0.35 : 1)
            .padding(.top, 4)

            Button {
                store.status = nil
                store.landingView = .menu
            } label: {
                Text(verbatim: "← \(lang.t("direct.back"))")
            }
            .buttonStyle(LinkButtonStyle())
            .disabled(connecting)
            .frame(maxWidth: .infinity) // .link-btn self-centers

            if let status {
                Text(verbatim: status)
                    .font(Theme.mono(13))
                    .foregroundStyle(Theme.inkSoft)
                    .frame(maxWidth: .infinity)
            }
        }
        .frame(maxWidth: .infinity)
    }

    private func fieldLabel(_ text: String) -> some View {
        Text(verbatim: text)
            .font(Theme.mono(11))
            .textCase(.uppercase)
            .kerning(0.9)
            .foregroundStyle(Theme.inkSoft)
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
        guard !connecting, !token.trimmed.isEmpty else { return }
        guard let base = normalizedUrl else {
            status = lang.t("direct.invalidUrl")
            return
        }
        connecting = true
        status = nil
        let attemptToken = token.trimmed
        Task {
            let failure = await store.directConnect(baseUrl: base, token: attemptToken)
            connecting = false
            status = failure
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
