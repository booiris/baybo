import Foundation

@testable import Baybo

/// One place to build an `LlmModelInfo` in tests.
///
/// The record grew from six fields to fourteen when the Settings entry editor
/// landed, and every hand-rolled fixture in the suite had to be touched. This
/// exists so the next field costs one default here instead: a test names only
/// what it is actually asserting on, and inherits a coherent entry for the rest.
///
/// The defaults describe the ORDINARY entry — everything inherited, a key that
/// resolves. A test that cares about a pinned override says so by name, which
/// is also what makes the inherited-vs-pinned assertions readable.
@MainActor
enum LlmFixtures {
    static func entry(
        _ name: String,
        provider: String = "anthropic",
        model: String,
        candidates: [String] = [],
        efforts: [String] = [],
        reasoningEffort: String? = nil,
        liteModel: String? = nil,
        baseUrl: String? = nil,
        apiKeyEnv: String? = nil,
        apiKeyConfigured: Bool = true,
        contextWindowOverride: UInt32? = nil,
        effectiveContextWindow: UInt32 = 200_000,
        supportsVisionOverride: Bool? = nil,
        effectiveSupportsVision: Bool = true
    ) -> LlmModelInfo {
        LlmModelInfo(
            name: name,
            provider: provider,
            model: model,
            modelCandidates: candidates,
            reasoningEffort: reasoningEffort,
            availableEfforts: efforts,
            liteModel: liteModel,
            baseUrl: baseUrl,
            apiKeyEnv: apiKeyEnv,
            apiKeyConfigured: apiKeyConfigured,
            contextWindowOverride: contextWindowOverride,
            effectiveContextWindow: effectiveContextWindow,
            supportsVisionOverride: supportsVisionOverride,
            effectiveSupportsVision: effectiveSupportsVision)
    }

    /// A green probe. `LlmTestResult` is all-optional on the success/failure
    /// split, so building one by hand at each call site invites a fixture that
    /// carries both an error and a latency — a shape the gateway never sends.
    static func probeOk(
        latencyMs: UInt64 = 812, provider: String = "anthropic", model: String = "claude-sonnet-5"
    ) -> LlmTestResult {
        LlmTestResult(
            ok: true, error: nil, latencyMs: latencyMs, inputTokens: 9, outputTokens: 3,
            provider: provider, model: model)
    }

    static func probeFailed(
        _ error: String, provider: String = "anthropic", model: String = "claude-sonnet-5"
    ) -> LlmTestResult {
        LlmTestResult(
            ok: false, error: error, latencyMs: nil, inputTokens: nil, outputTokens: nil,
            provider: provider, model: model)
    }
}
