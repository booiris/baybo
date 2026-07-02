//! Native chat header: a paper-gradient veil + controls pinned OVER the webview.
//!
//! Why native: the iOS keyboard reveals a focused web input by PANNING the
//! visual viewport over the layout viewport, and every web-side pinning
//! strategy (absolute, fixed, visual-viewport counter-translation, resizing
//! the webview frame) either rides off the visual top with that pan, lags it
//! visibly, or fights WebKit's own keyboard handling. A native sibling of the
//! webview sits outside the pan entirely, so this chrome holds the top no
//! matter what the keyboard does.
//!
//! Look: a translucent paper veil, no blur. iOS has no public API for a
//! subtle custom-radius blur — only the fixed-strength materials and the
//! paused-UIViewPropertyAnimator hack, which backgrounding kept flattening to
//! the full frosted material — so the veil alone does the work (a fraction
//! stronger to compensate). It's a CAGradientLayer in translucent paper
//! white, holding through the status-bar + bar zone and easing out over a
//! ramp below, so thread text dissolves gradually as it slides under
//! (mirroring the web header this replaces — `.chat-header` in styles.css).
//! Edge details that matter: every gradient stop is WHITE with decreasing
//! alpha (a `clearColor` endpoint fades through gray), and the tail follows a
//! smoothstep curve (a two-stop linear fade shows a visible crease where it
//! ends).
//!
//! Touch: the veil is non-interactive so scroll gestures in the ramp still
//! reach the thread; only the logout button (its own small sibling view)
//! takes touches. The webview drives the bar through the `native_header_set`
//! command (visibility, connection state, localized label), so copy/i18n stay
//! web-side; the logout button emits `native-logout` back to the webview,
//! which opens its existing confirm popover. The command returns whether a
//! native bar exists at all — the webview keeps its DOM header otherwise
//! (desktop/dev browsers).
//!
//! Main-thread discipline: the views are built on the main thread (Tauri's
//! `with_webview` runs there), stashed in a [`MainThreadBound`], and every
//! later mutation goes through `run_on_main_thread`.

#[cfg(target_os = "ios")]
mod ios {
    use std::sync::OnceLock;

    use dispatch2::MainThreadBound;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{MainThreadMarker, msg_send};
    use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
    use objc2_foundation::{NSArray, NSNumber, NSString};
    use objc2_ui_kit::{
        UIButton, UIButtonType, UIColor, UIControlState, UIFont, UILabel, UIView,
        UIViewAutoresizing,
    };
    use tauri::Emitter;

    /// Bar content height below the status-bar safe area — the iOS navigation
    /// bar's standard 44pt, which also matches the web header's tap targets.
    const BAR_CONTENT_HEIGHT: CGFloat = 44.0;
    /// Horizontal content margin; mirrors the web screen column's 1.25rem.
    const MARGIN: CGFloat = 20.0;
    /// Connection dot diameter; mirrors the web header's 0.55rem dot.
    const DOT: CGFloat = 9.0;
    /// Logout tap target (44pt iOS floor).
    const BUTTON: CGFloat = 44.0;
    /// Fade ramp below the bar content — the zone where thread text dissolves;
    /// mirrors the web header's ~2rem ramp.
    const RAMP: CGFloat = 36.0;

    /// The fade SHAPE per gradient stop: two full stops hold the solid zone
    /// (status bar + bar content), then the tail eases out along a smoothstep
    /// curve. Scaled by [`VEIL_PEAK_ALPHA`]; paired with
    /// [`FADE_TAIL_FRACTIONS`].
    const FADE_ALPHAS: [CGFloat; 7] = [1.0, 1.0, 0.9, 0.65, 0.35, 0.1, 0.0];
    /// Where each tail stop sits within the ramp (normalized 0..1 of the ramp).
    const FADE_TAIL_FRACTIONS: [CGFloat; 5] = [0.2, 0.4, 0.6, 0.8, 1.0];
    /// The white veil's peak opacity — translucent, but strong enough to wash
    /// the thread out under the bar without a blur layer helping.
    const VEIL_PEAK_ALPHA: CGFloat = 0.75;

    struct BarViews {
        veil: Retained<UIView>,
        gradient: Retained<AnyObject>,
        webview: Retained<UIView>,
        dot: Retained<UIView>,
        label: Retained<UILabel>,
        button: Retained<UIButton>,
    }

    static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
    static BAR: OnceLock<MainThreadBound<BarViews>> = OnceLock::new();

    /// `--color-ink` (#111111).
    fn ink() -> Retained<UIColor> {
        UIColor::colorWithRed_green_blue_alpha(0.067, 0.067, 0.067, 1.0)
    }
    /// `--color-ink-soft` (#6b6b6b).
    fn ink_soft() -> Retained<UIColor> {
        UIColor::colorWithRed_green_blue_alpha(0.42, 0.42, 0.42, 1.0)
    }
    /// `--color-err` (#d40000).
    fn err() -> Retained<UIColor> {
        UIColor::colorWithRed_green_blue_alpha(0.831, 0.0, 0.0, 1.0)
    }

    pub fn install(window: &tauri::WebviewWindow) {
        use tauri::Manager;

        if BAR.get().is_some() {
            return;
        }
        let _ = APP.set(window.app_handle().clone());
        let _ = window.with_webview(|webview| build(webview.inner().cast()));
    }

    /// A CAGradientLayer fading top→bottom through white stops at
    /// [`FADE_ALPHAS`] × `scale` (stop locations follow the layout in `apply`).
    fn make_fade_gradient(scale: CGFloat) -> Option<Retained<AnyObject>> {
        // SAFETY: CAGradientLayer is registered by QuartzCore at load; `+layer`
        // returns an autoreleased instance we retain. Raw msg_send keeps
        // objc2-quartz-core out of the deps for a handful of calls.
        let gradient = unsafe {
            let cls = AnyClass::get(c"CAGradientLayer")?;
            let layer: *mut AnyObject = msg_send![cls, layer];
            Retained::retain(layer)?
        };
        // Every stop is WHITE with decreasing alpha — see the module doc on why
        // the endpoint must not be `clearColor`.
        let colors = FADE_ALPHAS
            .iter()
            .map(|&alpha| {
                let color = UIColor::colorWithRed_green_blue_alpha(1.0, 1.0, 1.0, alpha * scale);
                // SAFETY: -[UIColor CGColor] returns the color's CGColorRef — a
                // CF object toll-free-bridged to ObjC, retainable and valid as
                // a CAGradientLayer colors element.
                unsafe {
                    let cg: *mut AnyObject = msg_send![&*color, CGColor];
                    Retained::retain(cg)
                }
            })
            .collect::<Option<Vec<_>>>()?;
        let colors = NSArray::from_retained_slice(&colors);
        // SAFETY: setColors takes an NSArray of CGColorRef.
        unsafe {
            let _: () = msg_send![&*gradient, setColors: &*colors];
        }
        Some(gradient)
    }

    fn build(webview: *mut AnyObject) {
        // `with_webview` runs its closure on the main thread.
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        // SAFETY: `inner()` hands us the live WKWebView (a UIView); retaining
        // keeps it valid for the app-lifetime chrome below.
        let Some(webview) = (unsafe { Retained::retain(webview.cast::<UIView>()) }) else {
            return;
        };
        let Some(superview) = webview.superview() else {
            return;
        };

        // The veil: a plain view carrying the translucent paper gradient.
        // Non-interactive, so drags across the ramp scroll the thread beneath.
        let veil = UIView::new(mtm);
        veil.setUserInteractionEnabled(false);
        veil.setAutoresizingMask(
            UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleBottomMargin,
        );
        // Hidden until the chat screen asks for it; frames + gradient stops are
        // (re)computed in `apply` — safe-area insets aren't settled this early.
        veil.setHidden(true);
        let Some(gradient) = make_fade_gradient(VEIL_PEAK_ALPHA) else {
            return;
        };
        // SAFETY: addSublayer parents the gradient under the veil's backing
        // layer; main thread.
        unsafe {
            let veil_layer: *mut AnyObject = msg_send![&*veil, layer];
            let _: () = msg_send![veil_layer, addSublayer: &*gradient];
        }

        let dot = UIView::new(mtm);
        veil.addSubview(&dot);

        let label = UILabel::new(mtm);
        // SAFETY: null font/color are valid "reset" arguments; we pass real ones.
        unsafe {
            label.setFont(Some(&UIFont::monospacedSystemFontOfSize_weight(
                13.0,
                objc2_ui_kit::UIFontWeightRegular,
            )));
            label.setTextColor(Some(&ink_soft()));
        }
        label.setAutoresizingMask(UIViewAutoresizing::FlexibleWidth);
        veil.addSubview(&label);

        // Logout: a bare line-art SF-symbol button; the tap round-trips through
        // the webview (`native-logout`) so the confirm flow stays web-side. Its
        // own SMALL sibling view (not a veil child) — the veil is
        // non-interactive so the ramp doesn't eat scroll gestures, and a child
        // of a non-interactive view couldn't be tapped.
        let handler =
            block2::RcBlock::new(|_action: core::ptr::NonNull<objc2_ui_kit::UIAction>| {
                if let Some(app) = APP.get() {
                    let _ = app.emit("native-logout", ());
                }
            });
        // SAFETY: the handler pointer is a live block; UIKit copies it.
        let action = unsafe {
            objc2_ui_kit::UIAction::actionWithHandler(
                (&*handler
                    as *const block2::DynBlock<dyn Fn(core::ptr::NonNull<objc2_ui_kit::UIAction>)>)
                    .cast_mut(),
                mtm,
            )
        };
        let button =
            UIButton::buttonWithType_primaryAction(UIButtonType::System, Some(&action), mtm);
        if let Some(image) = objc2_ui_kit::UIImage::systemImageNamed(objc2_foundation::ns_string!(
            "rectangle.portrait.and.arrow.right"
        )) {
            button.setImage_forState(Some(&image), UIControlState::Normal);
        }
        // SAFETY: a concrete tint color is always a valid argument.
        unsafe { button.setTintColor(Some(&ink())) };
        button.setAutoresizingMask(UIViewAutoresizing::FlexibleLeftMargin);
        button.setHidden(true);

        superview.insertSubview_aboveSubview(&veil, &webview);
        superview.insertSubview_aboveSubview(&button, &veil);

        let views = BarViews {
            veil,
            gradient,
            webview,
            dot,
            label,
            button,
        };
        let _ = BAR.set(MainThreadBound::new(views, mtm));
    }

    /// Webview-driven update: visibility, connection state, localized label.
    /// Returns whether a native bar exists (gates hiding the DOM header).
    pub fn set(app: &tauri::AppHandle, visible: bool, state: String, label: String) -> bool {
        if BAR.get().is_none() {
            return false;
        }
        let _ = app.run_on_main_thread(move || {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let Some(views) = BAR.get() else {
                return;
            };
            apply(views.get(mtm), visible, &state, &label);
        });
        true
    }

    fn apply(views: &BarViews, visible: bool, state: &str, label: &str) {
        views.veil.setHidden(!visible);
        views.button.setHidden(!visible);
        views.label.setText(Some(&NSString::from_str(label)));

        // Layout is recomputed on every update rather than at install: the
        // safe-area insets are only settled once the hierarchy has laid out,
        // and the first visible update always happens after that.
        let safe_top = views
            .webview
            .superview()
            .map(|s| s.safeAreaInsets().top)
            .unwrap_or(0.0);
        let width = views
            .webview
            .superview()
            .map(|s| s.bounds().size.width)
            .unwrap_or(0.0);
        let total = safe_top + BAR_CONTENT_HEIGHT + RAMP;
        let chrome_frame = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width,
                height: total,
            },
        };
        views.veil.setFrame(chrome_frame);

        // Gradient geometry: solid until the bar content ends, then the eased
        // tail spreads across the ramp. Locations are normalized to the veil's
        // height, which depends on the (now settled) safe area.
        let solid = (safe_top + BAR_CONTENT_HEIGHT) / total;
        let ramp = 1.0 - solid;
        let Some(locations) = [0.0, solid]
            .into_iter()
            .chain(FADE_TAIL_FRACTIONS.iter().map(|f| solid + f * ramp))
            .map(|l| {
                let n = NSNumber::numberWithDouble(l);
                // Upcast to AnyObject so one array type fits msg_send.
                unsafe { Retained::retain(Retained::as_ptr(&n) as *mut AnyObject) }
            })
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        let locations = NSArray::from_retained_slice(&locations);
        // SAFETY: CAGradientLayer setters with matching types, on the main
        // thread.
        unsafe {
            let _: () = msg_send![&*views.gradient, setLocations: &*locations];
            let _: () = msg_send![&*views.gradient, setFrame: chrome_frame];
        }

        views.dot.setFrame(CGRect {
            origin: CGPoint {
                x: MARGIN,
                y: safe_top + (BAR_CONTENT_HEIGHT - DOT) / 2.0,
            },
            size: CGSize {
                width: DOT,
                height: DOT,
            },
        });
        views.label.setFrame(CGRect {
            origin: CGPoint {
                x: MARGIN + DOT + 8.0,
                y: safe_top,
            },
            size: CGSize {
                width: (width - 2.0 * (MARGIN + DOT + 8.0 + BUTTON)).max(0.0),
                height: BAR_CONTENT_HEIGHT,
            },
        });
        views.button.setFrame(CGRect {
            origin: CGPoint {
                x: (width - MARGIN - BUTTON).max(0.0),
                y: safe_top,
            },
            size: CGSize {
                width: BUTTON,
                height: BAR_CONTENT_HEIGHT,
            },
        });

        // Dot styling per connection state, mirroring `.conn-state` in
        // styles.css: ink fill = connected, hollow = connecting, red = offline
        // (the app's only hue). The label goes red with it when offline.
        let (fill, border_width, label_color) = match state {
            "connected" => (Some(ink()), 0.0, ink_soft()),
            "offline" => (Some(err()), 0.0, err()),
            _ => (None, 1.0, ink_soft()),
        };
        views.dot.setBackgroundColor(fill.as_deref());
        // SAFETY: -[UIView layer] always returns a live CALayer; the corner /
        // border setters take CGFloat / CGColorRef. Raw msg_send keeps
        // objc2-quartz-core out of the deps.
        unsafe {
            let layer: *mut AnyObject = msg_send![&*views.dot, layer];
            let _: () = msg_send![layer, setCornerRadius: DOT / 2.0];
            let _: () = msg_send![layer, setBorderWidth: border_width as CGFloat];
            let border: *mut AnyObject = msg_send![&*ink_soft(), CGColor];
            let _: () = msg_send![layer, setBorderColor: border];
            views.label.setTextColor(Some(&label_color));
        }
    }
}

/// Show/update/hide the native chat header. Returns `true` when a native bar
/// exists (iOS with the bar installed) — the webview hides its DOM header then.
#[cfg(target_os = "ios")]
pub fn set(app: &tauri::AppHandle, visible: bool, state: String, label: String) -> bool {
    ios::set(app, visible, state, label)
}

#[cfg(not(target_os = "ios"))]
pub fn set(_app: &tauri::AppHandle, _visible: bool, _state: String, _label: String) -> bool {
    false
}

/// Build the header chrome over the main webview (hidden until the chat asks
/// for it). No-op off iOS; idempotent.
#[cfg(target_os = "ios")]
pub fn install(window: &tauri::WebviewWindow) {
    ios::install(window);
}

#[cfg(not(target_os = "ios"))]
pub fn install(_window: &tauri::WebviewWindow) {}
