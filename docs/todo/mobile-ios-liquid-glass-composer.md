# iOS 26 Liquid Glass composer dock + native jump-to-latest

**Status: IMPLEMENTED (2026-07-03)** — verified on the iPhone 17 Pro
simulator: at-rest glass dock, mid-scroll refraction + native jump button
round trip (`-baybo-demo-jump`, added as a DEBUG launch arg during this
change), and the keyboard ride (`-baybo-demo-keyboard` recording, no
transparent hole / white band). Two deltas from the plan as written:
(a) the dock does NOT use `.contentShape(Rectangle())` — the veil's base
`Color` is itself hit-testable (exact parity with the old opaque background),
with only the ramp overlay marked `allowsHitTesting(false)`; (b) the
geometry callback is `bridge.setComposerTop(minY)` (the bridge API had
evolved past the `setBottomInset` naming below); (c) a follow-up owner tweak
removed the mic placeholder outright (the field now spans to the right edge
and the `glassEffectID`/`@Namespace` morph scaffolding went with it), pinned
the pill's single-line height to 44pt flush with the paperclip circle, and
bumped the glyphs (paperclip 20pt medium, jump arrow 17pt). The rest landed
as specified. Kept for the design rationale and the invariants.

Every SwiftUI spelling below was typechecked against the local toolchain
(Xcode 26.6 / iphonesimulator SDK 26.5, `swiftc -typecheck -target
arm64-apple-ios26.0-simulator`); the layout and design-guide risks were
adversarially reviewed against the actual code.

**Scope:** the chat composer dock (`app/ios/App/Screens/ComposerView.swift`)
and the jump-to-latest button. Glass is **confined to these two surfaces**;
landing/login/scan screens and the header stay on the flat monochrome system.

## Locked decisions

1. **Field pill** → liquid glass (`.regular`, white ~50% tint, **no**
   `.interactive()` shimmer) **plus a retained 1px hairline**. Both the tint
   and the hairline are load-bearing, not cosmetic — see "Backdrop reality"
   below.
2. **Paperclip / mic icons** → glass circles with **ink glyphs**. This is a
   deliberate divergence from the Tauri sibling composer (ink-filled circles,
   paper glyphs); record it in `app/ios/CLAUDE.md`.
3. **Send button** → stays ink-black with a paper up-arrow, as an **explicit
   shape** (`Circle().fill(Theme.ink)` + `arrow.up`), replacing the
   `arrow.up.circle.fill` glyph which reads as a thin anti-aliased ring against
   glass specular.
4. **Jump-to-latest** → moves from the web bundle to a **native** glass circle
   above the composer; the web keeps the state machine and the glide, native
   keeps the pixels.
5. **`setBottomInset` semantics unchanged** (full covered strip). The
   "tuck content under the glass" variant is explicitly out of scope (v2, see
   bottom).
6. The dock's opaque paper background is replaced by a **bottom paper veil**
   (mirror of the header veil) + an explicit hit-test shape — never raw
   transparency.

## SDK facts (all typechecked locally)

Deployment target is already iOS 26.0 (`app/ios/project.yml`) — no
availability gating anywhere.

Verified compiling:

- `GlassEffectContainer(spacing: 8) { ... }` — `init(spacing: CGFloat? = nil, @ViewBuilder content:)`
- `.glassEffect(.regular.interactive(), in: .circle)` — `.circle` shorthand ok; `interactive(_ isEnabled: Bool = true)`
- `.glassEffect(.regular.tint(.white.opacity(0.5)), in: .rect(cornerRadius: 22))` — `Glass.tint(_ color: Color?)`
- `.glassEffect(.regular, in: RoundedRectangle(cornerRadius: 22))` — `in shape: some Shape` is generic
- `@Namespace` + `.glassEffectID("mic", in: ns)`
- `Glass` variants: `.regular`, `.clear`, `.identity`; `.buttonStyle(.glass)` / `.glassProminent` exist (unused here)

**Rejected by the SDK:** `glassEffect(..., isEnabled:)` — no such parameter
("extra argument 'isEnabled' in call"). Conditional glass = ternary on the
variant: `.glassEffect(on ? .regular : .identity, in: ...)`.

Probe files (rebuildable): scratchpad `glass_probe.swift` / `glass_probe2.swift`.

## Backdrop reality (why tint + hairline + veil are required)

The transcript webview is full-bleed under the dock
(`ChatScreen.swift:30-31`), **but** the web thread pads its scroll extent by
the full obstruction (`--thread-bottom-inset` + 1rem,
`app/ios/web/src/styles.css:72`). So:

- **At rest** (pinned to the newest edge — the default state, and all of
  streaming) the strip behind the dock is the thread's own blank white
  padding. Untinted glass over white is nearly invisible; a borderless glass
  field would have no boundary at all.
- **Mid-scroll into history** is the only time content (ink user bubbles)
  passes beneath the dock — glass earns its keep exactly then, which is also
  exactly when the jump-to-latest button is visible.

Hence: white-tinted glass + 1px `lineStrong` hairline on the field (also the
sibling Tauri composer's precedent — it strengthens its border to `#bcbcbc`
over frost, `app/mobile/src/styles.css:572-587`), and a paper veil behind the
dock so the notice row / staged strip never float on raw transcript content.

## Mechanical plan

### 1. `App/Support/Theme.swift`

```swift
static let lineStrong = Color(red: 0xBC / 255.0, green: 0xBC / 255.0, blue: 0xBC / 255.0)
```

(Mirrors Tauri `--color-line-strong` — "hairlines that must hold up over
busy/frosted backdrops".)

### 2. `App/Screens/ComposerView.swift`

Add `@Namespace private var glassNS`. Wrap the bottom `HStack` (line 47) in
`GlassEffectContainer(spacing: 8)`.

**Paperclip** (PhotosPicker label) and **mic** — 44pt (HIG floor now that the
circle is visible; was 40):

```swift
Image(systemName: "paperclip")
    .font(.system(size: 18))
    .foregroundStyle(Theme.ink)
    .frame(width: 44, height: 44)
    .glassEffect(.regular.interactive(), in: .circle)
    .glassEffectID("clip", in: glassNS)      // mic: "mic"
```

Add `.animation(.default, value: hasDraft)` on the container content so the
mic's collapse/appearance morphs (glassEffectID merge) instead of popping.

**Field pill** — replace the `background`/`overlay` pair at lines 76–83:

```swift
.glassEffect(.regular.tint(Theme.paper.opacity(0.5)), in: .rect(cornerRadius: 22))
.overlay(RoundedRectangle(cornerRadius: 22).strokeBorder(Theme.lineStrong, lineWidth: 1))
.glassEffectID("field", in: glassNS)
```

No `.interactive()` on the field. (Optional polish, decide at build time:
flip the stroke to `Theme.ink` while `focused` — Tauri does.)

**Send button** — replace the glyph at lines 67–69:

```swift
Circle()
    .fill(hasDraft ? Theme.ink : Theme.line)
    .frame(width: 34, height: 34)
    .overlay(
        Image(systemName: "arrow.up")
            .font(.system(size: 15, weight: .semibold))
            .foregroundStyle(Theme.paper))
```

Keep `.disabled(!hasDraft)` and the accessibility label. Trim its
`.padding(.trailing/.bottom)` from 6 to 4 so the single-line pill stays ~42pt
(the circle is 8pt taller than the old glyph).

**Dock chrome** — replace `.background(Theme.paper)` (line 99) with:

```swift
.contentShape(Rectangle())
.background { composerVeil }
```

- `.contentShape(Rectangle())` is a **hard requirement**, not styling: the
  old opaque `Color` background was what made the whole dock rect
  hit-testable. Without a replacement, taps/drags in the 12pt gutters, the
  8pt top pad, and inter-item gaps fall through to the WKWebView — scrolling
  the transcript and triggering interactive keyboard dismiss
  (`TranscriptWebView.swift:31`).
- `composerVeil` — bottom mirror of the header veil (`ChatScreen.swift:90-91`
  peak/ramp grammar), sized to the dock automatically:

```swift
private var composerVeil: some View {
    VStack(spacing: 0) {
        LinearGradient(
            stops: [                       // smoothstep tail, header's alphas reversed
                .init(color: .white.opacity(0.0),  location: 0),
                .init(color: .white.opacity(0.08), location: 0.2),
                .init(color: .white.opacity(0.28), location: 0.4),
                .init(color: .white.opacity(0.52), location: 0.6),
                .init(color: .white.opacity(0.72), location: 0.8),
                .init(color: .white.opacity(0.8),  location: 1.0),
            ],
            startPoint: .top, endPoint: .bottom
        )
        .frame(height: 36)
        Color.white.opacity(0.8)
    }
    .padding(.top, -36)                    // tail overflows above the dock
    .ignoresSafeArea(edges: .bottom)       // covers the home-indicator strip
    .allowsHitTesting(false)
}
```

The veil solves four things at once: legibility of the red notice row + the
staged strip over scrolled content; a finished home-indicator zone; masking
the pre-existing web-vs-native animation phase mismatch (below); and it still
lets ink bubbles ghost through the glass at ~20% during scroll.

### 3. `App/Screens/ChatScreen.swift` — native jump button

```swift
.safeAreaInset(edge: .bottom, spacing: 0) {
    VStack(spacing: 12) {
        if bridge.jumpVisible {
            Button { bridge.jumpToLatest() } label: {
                Image(systemName: "arrow.down")
                    .font(.system(size: 15, weight: .medium))
                    .foregroundStyle(Theme.ink)
                    .frame(width: 44, height: 44)
            }
            .glassEffect(.regular.interactive(), in: .circle)
            .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.jumpToLatest")))
            .transition(.scale(scale: 0.7).combined(with: .opacity))   // = web jump-pop
        }
        ComposerView(store: store)
            .onGeometryChange(...)          // unchanged, and MUST stay on ComposerView
    }
    .animation(.easeOut(duration: 0.16), value: bridge.jumpVisible)
}
```

**Invariant — the trap to not fall into:** `onGeometryChange` stays attached
to `ComposerView`, never the wrapping `VStack`. The measured strip must
exclude the jump button, or `--thread-bottom-inset` jumps ~56px every time the
button pops in while scrolling. The webview ignores safe areas entirely, so
the inset view growing taller has no other effect.

The button is deliberately **outside** the composer's
`GlassEffectContainer` — it is spatially separate and must not merge.

### 4. `App/Web/TranscriptBridge.swift`

- `@Published private(set) var jumpVisible = false`.
- `handle(type:body:)` — new case, hopping to main like the existing cases:
  `case "jumpVisible": jumpVisible = (body["visible"] as? Bool) ?? false`.
  Reset it to `false` in the same place the bridge handles `ready`/reload
  re-init, so a webview reload can't strand a stale button.
- Native→web:
  `func jumpToLatest() { call("jumpToLatest", "") }` — `call` assembles
  `window.baybo && window.baybo.jumpToLatest();`, buffering pre-`ready` as
  usual.

### 5. `web/src/bridge.ts`

- `BayboGlobal` += `jumpToLatest(): void`; implement as
  `dispatch({ kind: "jumpToLatest" })`.
- `Buffered` += `{ kind: "jumpToLatest" }`; `TranscriptEvents` +=
  `jumpToLatest(): void`; extend `deliver`.
- `export function postJumpVisible(visible: boolean): void` →
  `postSafe({ type: "jumpVisible", visible })`.

### 6. `web/src/Transcript.tsx`

- Keep the entire `showJump` state machine (onScroll follow-band tracking,
  glide flag, `GLIDE_SETTLE_CAP_MS`) and the `jumpToLatest()` function
  untouched.
- Mirror the state out: `useEffect(() => { postJumpVisible(showJump); },
  [showJump])`.
- Subscribe the native tap: add `jumpToLatest: () =>
  handlersRef.current.handleJumpToLatest()` to the `subscribeTranscript`
  events (same `handlersRef` pattern as `bottomInset`), pointing at the
  existing function.
- Delete the button JSX (lines ~835–860) including the
  `onPointerDown/onMouseDown preventDefault` keyboard-focus hack — a native
  button never touches webview focus, so the hack retires with it.

### 7. `web/src/styles.css`

Delete `.jump-latest` and friends (lines ~319–353: base rule, `svg`,
`:active`, the `::after` hit-slop, `@keyframes jump-pop`).

### 8. i18n

- Add `chat.jumpToLatest` to `App/Resources/Localizable.xcstrings`
  (en "Jump to the latest message" / zh-Hans "跳到最新消息" — copy from the web
  locales).
- Remove the now-dead key from `web/src/locales/en.ts` / `zh.ts`.

### 9. Docs

`app/ios/CLAUDE.md`:

- Bridge lists: native→web += `jumpToLatest`; web→native += `jumpVisible`.
- Add a visual-system note: Liquid Glass is adopted for the chat composer
  dock + jump-to-latest only (white tint, no shimmer on the field, hairline
  retained, glass icon circles with ink glyphs — a recorded divergence from
  the Tauri composer's ink-filled circles); everything else stays on
  `app/mobile/CLAUDE.md` flat monochrome.

## Known-and-accepted behaviors (do not "fix" en route)

- **Glass reads as paper at rest.** By design (inset semantics unchanged);
  content refracts only mid-scroll. Not a bug.
- **Keyboard-slide phase mismatch is pre-existing.** The web animates padding
  on its own 250ms curve from a start-of-animation bridge message while the
  dock rides the real keyboard curve; the opaque dock used to mask the
  mismatch, the veil now masks it. If a faint white band flickers through the
  glass during raise/dismiss, raise the veil's peak alpha rather than touching
  the inset contract.
- **Composer growth (notice line / staged strip) has the same 250ms lag** —
  same masking, same answer.
- **`jumpVisible` ordering** rides the existing FIFO postMessage path; the web
  already zeroes `showJump` at glide start, so the native button hides
  immediately on tap.

## Verification

1. `scripts/build.sh`, then `-baybo-open-chat -baybo-demo-frames`: screenshot
   ~3s/~6s/~12s — glass pill + icons render, send circle states (gray→ink)
   correct, veil holds the notice row legible.
2. Scroll up into history: ink bubbles ghost under the dock; native jump
   button pops (scale+fade), tap glides to newest and the button hides; verify
   the thread inset does NOT jump when the button appears (invariant §3).
3. `-baybo-demo-keyboard` + `simctl io recordVideo`, extract frames: composer
   rides the keyboard, no transparent hole, no unacceptable flicker band
   behind the glass; keyboard stays up when tapping the jump button mid-edit.
4. Attachments: stage an image — strip legible over the veil; error notice
   (oversized pick) legible.
5. `(cd app/ios/web && pnpm build)` — tsc catches any bridge typing slip;
   full checklist item 5 from `app/ios/CLAUDE.md` on device.

## Out of scope (v2 candidates)

- **Tuck-under inset**: shrinking the web-side inset so the newest messages
  sit beneath the glass at rest. Requires a second CSS variable (the current
  one also anchors the home-indicator contract, `styles.css:27-32`) and
  exposes the keyboard phase mismatch — needs its own design pass.
- `glassEffectUnion` grouping of clip+field+mic into one glass shape.
- Any glass on landing/login/scan or the header.
