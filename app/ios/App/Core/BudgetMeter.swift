import Foundation

/// A board's two daily ceilings, and how they are said.
///
/// A board has **two** — money and tokens — and it stops when either is
/// reached. Money alone was never enough: on a subscription plan every cost
/// record is zero, so a money ceiling can never be reached however low it is
/// set, and that is the ordinary case rather than an exotic one.
///
/// A hold is a **standing condition, not news**. It does not arrive, and it
/// stops being true only when the operator changes a number — so it belongs
/// next to the setting that lifts it, never in a red dot that cannot be
/// cleared by looking. Painting it as one is how it got reported: "the red
/// dot won't go away", and the operator was right.
enum BudgetMeter {
    /// Where a board sits against whichever ceiling is biting.
    enum Burn: Equatable {
        case none
        case under
        case over
    }

    struct Meter: Equatable {
        let spent: String
        let limit: String
        let burn: Burn
        /// Which ceiling this meter speaks in, for the wording around it.
        let ceiling: MoveConsequence.HeldCeiling
    }

    /// The UTC midnight a board's day starts at — **not** the device's.
    ///
    /// The ceiling measures a UTC window, so a meter computed from local
    /// midnight would accuse a board of crossing a ceiling it did not, and
    /// exonerate one that had.
    static func dayStartMs(now: Date = Date()) -> Int64 {
        var utc = Calendar(identifier: .gregorian)
        utc.timeZone = TimeZone(identifier: "UTC") ?? .gmt
        let start = utc.startOfDay(for: now)
        return Int64(start.timeIntervalSince1970 * 1000)
    }

    /// The one meter to show, when both ceilings are set.
    ///
    /// **Whichever has least room left speaks** — exhaustion first, then the
    /// tighter fraction, ties to tokens — which is the same rule the server
    /// picks its timeline entry by. "A set token ceiling always wins" was
    /// tried and is wrong: it stamps a token verdict on a board held by
    /// money, claiming a ceiling was spent at 0% used.
    static func meter(
        burnMicros: Int64,
        burnTokens: Int64,
        limitMicros: Int64?,
        limitTokens: Int64?
    ) -> Meter? {
        let money = limitMicros.map {
            Meter(
                spent: usd(burnMicros), limit: usd($0),
                burn: state(spent: burnMicros, limit: $0), ceiling: .money)
        }
        let tokens = limitTokens.map {
            Meter(
                spent: compactCount(burnTokens), limit: compactCount($0),
                burn: state(spent: burnTokens, limit: $0), ceiling: .tokens)
        }
        switch (money, tokens) {
        case let (.some(money), .some(tokens)):
            if money.burn == .over && tokens.burn != .over { return money }
            if tokens.burn == .over && money.burn != .over { return tokens }
            let moneyRoom = fraction(spent: burnMicros, limit: limitMicros ?? 0)
            let tokenRoom = fraction(spent: burnTokens, limit: limitTokens ?? 0)
            return moneyRoom > tokenRoom ? money : tokens
        case let (.some(money), .none): return money
        case let (.none, .some(tokens)): return tokens
        case (.none, .none): return nil
        }
    }

    /// Both meters, for the settings screen. Money and tokens are independent
    /// gates and either stops the board, so an operator shown only the tighter
    /// of two that are both spent raises one and watches nothing happen.
    static func meters(
        burnMicros: Int64,
        burnTokens: Int64,
        limitMicros: Int64?,
        limitTokens: Int64?
    ) -> [Meter] {
        [
            limitMicros.map {
                Meter(
                    spent: usd(burnMicros), limit: usd($0),
                    burn: state(spent: burnMicros, limit: $0), ceiling: .money)
            },
            limitTokens.map {
                Meter(
                    spent: compactCount(burnTokens), limit: compactCount($0),
                    burn: state(spent: burnTokens, limit: $0), ceiling: .tokens)
            },
        ].compactMap { $0 }
    }

    private static func fraction(spent: Int64, limit: Int64) -> Double {
        limit <= 0 ? 1 : Double(spent) / Double(limit)
    }

    private static func state(spent: Int64, limit: Int64) -> Burn {
        if limit <= 0 { return .none }
        return spent >= limit ? .over : .under
    }

    /// `$6.10`. Micro-USD in, two decimals out — the figure the operator
    /// compares against a ceiling they typed in dollars.
    static func usd(_ micros: Int64) -> String {
        let dollars = Double(micros) / 1_000_000
        return String(format: "$%.2f", dollars)
    }

    /// `602k`, `2M`. A token ceiling is set in round numbers and read at a
    /// glance; the exact count belongs nowhere on a phone.
    static func compactCount(_ value: Int64) -> String {
        let magnitude = abs(value)
        if magnitude >= 1_000_000 {
            let millions = Double(value) / 1_000_000
            return millions == millions.rounded()
                ? "\(Int(millions))M" : String(format: "%.1fM", millions)
        }
        if magnitude >= 1000 {
            return "\(value / 1000)k"
        }
        return "\(value)"
    }
}
