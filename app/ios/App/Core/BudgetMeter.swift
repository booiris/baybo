import Foundation

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

    static func dayStartMs(now: Date = Date()) -> Int64 {
        var utc = Calendar(identifier: .gregorian)
        utc.timeZone = TimeZone(identifier: "UTC") ?? .gmt
        let start = utc.startOfDay(for: now)
        return Int64(start.timeIntervalSince1970 * 1000)
    }

    static func meter(
        burnMicros: Int64,
        burnTokens: Int64,
        limitMicros: Int64?,
        limitTokens: Int64?
    ) -> Meter? {
        // Money and tokens are independent ceilings; show whichever is already
        // exceeded, otherwise whichever has the least remaining room.
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
