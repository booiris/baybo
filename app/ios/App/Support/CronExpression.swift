import Foundation

/// A cron expression, said in words.
///
/// `0 9 * * *` is not a schedule to most people; it is punctuation. This turns
/// the common shapes into a sentence in the app's language, and — the rule
/// everything here obeys — **returns the raw expression untouched for anything
/// it cannot describe exactly**. A list that says "daily at 9" for a schedule
/// that does something else is worse than one that shows the cron.
///
/// ## The day-of-week trap
///
/// Our scheduler is Quartz-numbered: **1=Sunday … 7=Saturday**, NOT the Unix
/// `0=Sunday/1=Monday` that most cron documentation teaches. So `* * * * 1-5` is
/// **Sunday..Thursday** here, and this renders it as such — the point is to say
/// what the gateway will actually do, not what the author hoped. (Unix's `0` is
/// not a silent mis-day: it fails to parse server-side.) Pinned by
/// `days_of_week_are_quartz_numbered_from_sunday` in `crates/cron`.
///
/// Happily, `Calendar`'s `weekday` is numbered the same way, so an ordinal
/// indexes `weekdaySymbols` directly and every day name comes from the OS in the
/// user's language rather than a hand-written table.
///
/// ## One language in, everything follows it
///
/// `lproj` names the language for BOTH halves of the sentence: the template and
/// the day names / clock the OS formats into it. Reading the template from
/// `Lang.shared` instead would let the two disagree — production would be fine
/// (a row is always rendered in the app's own language) but a test would silently
/// depend on the HOST machine's, which is the "passes locally, fails on the
/// runner" trap this repo has already paid for once.
enum CronExpression {
    /// The expression in words, or `expr` itself when it is not one of the
    /// shapes below.
    static func humanize(_ expr: String, lproj: String) -> String {
        guard let fields = Fields(expr) else { return expr }
        return describe(fields, lproj: lproj) ?? expr
    }

    /// The six fields our scheduler evaluates, seconds first (its `cron` crate is
    /// Quartz-flavoured). A 5-field expression — what people write, and what the
    /// gateway itself normalizes — implies second 0.
    private struct Fields {
        let second: String
        let minute: String
        let hour: String
        let dayOfMonth: String
        let month: String
        let dayOfWeek: String

        init?(_ expr: String) {
            let f = expr.split(whereSeparator: \.isWhitespace).map(String.init)
            switch f.count {
            case 5: (second, minute, hour, dayOfMonth, month, dayOfWeek) =
                ("0", f[0], f[1], f[2], f[3], f[4])
            case 6: (second, minute, hour, dayOfMonth, month, dayOfWeek) =
                (f[0], f[1], f[2], f[3], f[4], f[5])
            // 7 fields adds a year, which no sentence below accounts for.
            default: return nil
            }
        }
    }

    private static func describe(_ f: Fields, lproj: String) -> String? {
        // Anything sub-minute, or restricted to certain months, is past what
        // these sentences can say without lying by omission.
        guard f.second == "0", f.month == "*" else { return nil }
        // Cron ORs day-of-month with day-of-week when both are restricted — a
        // rule almost nobody means, and one no short sentence conveys.
        let byMonthDay = f.dayOfMonth != "*"
        let byWeekday = f.dayOfWeek != "*" && f.dayOfWeek != "?"
        if byMonthDay && byWeekday { return nil }

        if byWeekday {
            guard let time = clock(f, lproj: lproj), let days = weekdays(f.dayOfWeek, lproj: lproj)
            else { return nil }
            return t("cron.weekly", lproj, days, time)
        }
        if byMonthDay {
            guard let time = clock(f, lproj: lproj), let day = Int(f.dayOfMonth) else { return nil }
            return t("cron.monthly", lproj, String(day), time)
        }
        // Every day, so the sentence is only about the time of day.
        if let time = clock(f, lproj: lproj) {
            return t("cron.daily", lproj, time)
        }
        if f.hour == "*" {
            if let minute = Int(f.minute) {
                return minute == 0
                    ? t("cron.hourly", lproj)
                    : t("cron.hourlyAt", lproj, String(minute))
            }
            if f.minute == "*" { return t("cron.everyMinute", lproj) }
            if let step = step(f.minute) { return t("cron.everyMinutes", lproj, String(step)) }
        }
        // Every N hours, on the hour.
        if let step = step(f.hour), let minute = Int(f.minute), minute == 0 {
            return t("cron.everyHours", lproj, String(step))
        }
        return nil
    }

    private static func t(_ key: String, _ lproj: String) -> String {
        Lang.catalog(lproj: lproj).localizedString(forKey: key, value: key, table: nil)
    }

    private static func t(_ key: String, _ lproj: String, _ arg: String) -> String {
        String(format: t(key, lproj), arg)
    }

    /// Two substitutions, so the format MUST use positional specifiers
    /// (`%1$@`/`%2$@`): the order is the sentence's, and a translation is exactly
    /// where it changes.
    private static func t(_ key: String, _ lproj: String, _ a: String, _ b: String) -> String {
        String(format: t(key, lproj), a, b)
    }

    /// The time of day, when both hour and minute name a single one. `nil` for
    /// anything repeating within the day — the caller has other sentences for
    /// that.
    private static func clock(_ f: Fields, lproj: String) -> String? {
        guard let hour = Int(f.hour), let minute = Int(f.minute),
            (0...23).contains(hour), (0...59).contains(minute)
        else { return nil }
        var components = DateComponents()
        components.hour = hour
        components.minute = minute
        var calendar = Calendar(identifier: .gregorian)
        // A fixed zone: this is a wall clock the job carries, not an instant, so
        // it must not be shifted by wherever the phone happens to be.
        guard let utc = TimeZone(identifier: "UTC") else { return nil }
        calendar.timeZone = utc
        guard let date = calendar.date(from: components) else { return nil }
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: lproj)
        formatter.timeZone = utc
        formatter.setLocalizedDateFormatFromTemplate("jm")
        return formatter.string(from: date)
    }

    /// `6` / `FRI` / `2-6` / `2,4,6` as a localized day list. `nil` for anything
    /// else (a step, an `L`, an `#`), which sends the whole row back to raw.
    private static func weekdays(_ field: String, lproj: String) -> String? {
        var ordinals: [Int] = []
        for part in field.split(separator: ",") {
            let bounds = part.split(separator: "-", maxSplits: 1)
            switch bounds.count {
            case 1:
                guard let day = ordinal(String(bounds[0])) else { return nil }
                ordinals.append(day)
            case 2:
                guard let from = ordinal(String(bounds[0])), let to = ordinal(String(bounds[1])),
                    from <= to
                else { return nil }
                ordinals.append(contentsOf: from...to)
            default:
                return nil
            }
        }
        guard !ordinals.isEmpty else { return nil }

        let swiftLocale = Locale(identifier: lproj)
        var calendar = Calendar(identifier: .gregorian)
        calendar.locale = swiftLocale
        // Both this and `Calendar.weekday` count 1=Sunday, so the ordinal is the
        // index — no conversion, and none to get backwards.
        let symbols = calendar.shortWeekdaySymbols
        guard ordinals.allSatisfy({ (1...symbols.count).contains($0) }) else { return nil }

        // Deduplicated and ordered, so `6,2,2` reads as one sane list.
        let names = Set(ordinals).sorted().map { symbols[$0 - 1] }
        let list = ListFormatter()
        list.locale = swiftLocale
        return list.string(from: names) ?? names.joined(separator: " ")
    }

    /// A single day, by number (1=Sunday) or by the name our scheduler accepts.
    private static func ordinal(_ token: String) -> Int? {
        if let n = Int(token) { return (1...7).contains(n) ? n : nil }
        return Self.names[token.lowercased()]
    }

    /// The scheduler's own spellings (`crates/cron`'s `ordinal_from_name`),
    /// values included, so a rename on either side is a mismatch here rather than
    /// a wrong day in the UI.
    private static let names: [String: Int] = [
        "sun": 1, "sunday": 1,
        "mon": 2, "monday": 2,
        "tue": 3, "tues": 3, "tuesday": 3,
        "wed": 4, "wednesday": 4,
        "thu": 5, "thurs": 5, "thursday": 5,
        "fri": 6, "friday": 6,
        "sat": 7, "saturday": 7,
    ]

    /// The N of a bare `*/N` — and only that: `5/N` (from 5) and `1-9/N` (within
    /// a range) mean something this does not say.
    private static func step(_ field: String) -> Int? {
        let parts = field.split(separator: "/", maxSplits: 1)
        guard parts.count == 2, parts[0] == "*", let n = Int(parts[1]), n > 1 else { return nil }
        return n
    }
}
