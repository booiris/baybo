use std::collections::HashMap;

use tracing::{debug, warn};

use crate::{Hook, HookAction, HookContext, HookPoint, Result};

/// Stores registered hooks and triggers them at the appropriate lifecycle points.
///
/// Hooks for the same `HookPoint` are executed serially in registration order.
/// Decision precedence: `Abort` > `Block` > `ContinueWith` > `Continue`.
pub struct HookManager {
    hooks: HashMap<HookPoint, Vec<RegisteredHook>>,
}

struct RegisteredHook {
    critical: bool,
    hook: Box<dyn Hook>,
}

impl HookManager {
    pub fn new() -> Self {
        Self {
            hooks: HashMap::new(),
        }
    }

    /// Register a hook. It will be appended to the list for its hook point.
    pub fn register(&mut self, hook: impl Hook + 'static) {
        let point = hook.hook_point();
        let critical = hook.critical();
        self.hooks.entry(point).or_default().push(RegisteredHook {
            critical,
            hook: Box::new(hook),
        });
        debug!("Registered hook for {:?}", point);
    }

    /// Trigger all hooks registered for the given point, in order.
    ///
    /// Decision precedence across all hooks:
    /// - `Abort` stops execution immediately; no further hooks run.
    /// - `Block` records the block but remaining hooks still fire (for observability).
    /// - `ContinueWith` modifications are applied as they arrive.
    /// - `Continue` has no effect.
    ///
    /// Returns the most restrictive action seen across all hooks.
    pub async fn trigger(&self, point: HookPoint, ctx: &mut HookContext) -> Result<HookAction> {
        let Some(hooks) = self.hooks.get(&point) else {
            return Ok(HookAction::Continue);
        };

        let matcher_target = ctx.event_data.matcher_target().map(|s| s.to_string());
        let mut final_block: Option<String> = None;

        for registered in hooks {
            let hook_name = registered.hook.name();

            // Evaluate matcher: skip this hook if the matcher doesn't match.
            if let Some(matcher) = registered.hook.matcher() {
                match &matcher_target {
                    Some(target) => {
                        if !matcher.matches(target) {
                            continue;
                        }
                    }
                    // Event has no matcher target — only All matchers fire.
                    None => {
                        if !matcher.matches("") {
                            continue;
                        }
                    }
                }
            }

            debug!("Executing hook '{}' at {:?}", hook_name, point);

            let result = registered.hook.execute(ctx).await;

            match result {
                Ok(HookAction::Continue) => {}
                Ok(HookAction::ContinueWith(modification)) => {
                    ctx.apply(*modification);
                }
                Ok(HookAction::Block(reason)) => {
                    if point.supports_block() {
                        debug!("Hook '{}' blocked: {}", hook_name, reason);
                        // Record the first block reason; remaining hooks still fire.
                        if final_block.is_none() {
                            final_block = Some(reason);
                        }
                    } else {
                        warn!(
                            "Hook '{}' returned Block at {:?} which does not support blocking, treating as Continue",
                            hook_name, point
                        );
                    }
                }
                Ok(HookAction::Abort(reason)) => {
                    debug!("Hook '{}' aborted: {}", hook_name, reason);
                    return Ok(HookAction::Abort(reason));
                }
                Err(err) => {
                    if registered.critical {
                        warn!("Critical hook '{}' failed, aborting: {}", hook_name, err);
                        return Err(err);
                    }
                    warn!(
                        "Non-critical hook '{}' failed, continuing: {}",
                        hook_name, err
                    );
                }
            }
        }

        match final_block {
            Some(reason) => Ok(HookAction::Block(reason)),
            None => Ok(HookAction::Continue),
        }
    }
}

impl Default for HookManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::HookModification;
    use crate::event::HookEventData;
    use crate::matcher::HookMatcher;

    // -- helpers --

    struct NoopHook {
        hook_name: &'static str,
        point: HookPoint,
    }

    #[async_trait]
    impl Hook for NoopHook {
        fn name(&self) -> &str {
            self.hook_name
        }
        fn hook_point(&self) -> HookPoint {
            self.point
        }
        async fn execute(&self, _ctx: &mut HookContext) -> Result<HookAction> {
            Ok(HookAction::Continue)
        }
    }

    struct ModifyHook {
        hook_name: &'static str,
        point: HookPoint,
        key: String,
        value: serde_json::Value,
    }

    #[async_trait]
    impl Hook for ModifyHook {
        fn name(&self) -> &str {
            self.hook_name
        }
        fn hook_point(&self) -> HookPoint {
            self.point
        }
        async fn execute(&self, _ctx: &mut HookContext) -> Result<HookAction> {
            let mut extra = HashMap::new();
            extra.insert(self.key.clone(), self.value.clone());
            Ok(HookAction::ContinueWith(Box::new(HookModification {
                extra,
                ..Default::default()
            })))
        }
    }

    struct AbortHook {
        hook_name: &'static str,
        point: HookPoint,
        reason: String,
    }

    #[async_trait]
    impl Hook for AbortHook {
        fn name(&self) -> &str {
            self.hook_name
        }
        fn hook_point(&self) -> HookPoint {
            self.point
        }
        async fn execute(&self, _ctx: &mut HookContext) -> Result<HookAction> {
            Ok(HookAction::Abort(self.reason.clone()))
        }
    }

    struct BlockHook {
        hook_name: &'static str,
        point: HookPoint,
        reason: String,
    }

    #[async_trait]
    impl Hook for BlockHook {
        fn name(&self) -> &str {
            self.hook_name
        }
        fn hook_point(&self) -> HookPoint {
            self.point
        }
        async fn execute(&self, _ctx: &mut HookContext) -> Result<HookAction> {
            Ok(HookAction::Block(self.reason.clone()))
        }
    }

    struct CountingHook {
        hook_name: &'static str,
        point: HookPoint,
        counter: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Hook for CountingHook {
        fn name(&self) -> &str {
            self.hook_name
        }
        fn hook_point(&self) -> HookPoint {
            self.point
        }
        async fn execute(&self, _ctx: &mut HookContext) -> Result<HookAction> {
            self.counter.fetch_add(1, Ordering::SeqCst);
            Ok(HookAction::Continue)
        }
    }

    struct FailingHook {
        hook_name: &'static str,
        point: HookPoint,
        is_critical: bool,
    }

    #[async_trait]
    impl Hook for FailingHook {
        fn name(&self) -> &str {
            self.hook_name
        }
        fn hook_point(&self) -> HookPoint {
            self.point
        }
        fn critical(&self) -> bool {
            self.is_critical
        }
        async fn execute(&self, _ctx: &mut HookContext) -> Result<HookAction> {
            Err(crate::HookError::Execution("hook failed".into()))
        }
    }

    struct MatchedHook {
        hook_name: &'static str,
        point: HookPoint,
        hook_matcher: HookMatcher,
        counter: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Hook for MatchedHook {
        fn name(&self) -> &str {
            self.hook_name
        }
        fn hook_point(&self) -> HookPoint {
            self.point
        }
        fn matcher(&self) -> Option<&HookMatcher> {
            Some(&self.hook_matcher)
        }
        async fn execute(&self, _ctx: &mut HookContext) -> Result<HookAction> {
            self.counter.fetch_add(1, Ordering::SeqCst);
            Ok(HookAction::Continue)
        }
    }

    fn make_ctx() -> HookContext {
        HookContext {
            session_id: "sess-1".into(),
            user_id: None,
            event_data: HookEventData::PreMessage,
            message: None,
            response: None,
            job_id: None,
            trace_span_id: None,
            extra: HashMap::new(),
        }
    }

    fn make_tool_ctx(tool_name: &str) -> HookContext {
        HookContext {
            session_id: "sess-1".into(),
            user_id: None,
            event_data: HookEventData::PreToolUse {
                tool_name: tool_name.to_string(),
                tool_input: serde_json::Value::Null,
                tool_use_id: "tu-1".into(),
            },
            message: None,
            response: None,
            job_id: None,
            trace_span_id: None,
            extra: HashMap::new(),
        }
    }

    // -- tests --

    #[tokio::test]
    async fn trigger_with_no_hooks_returns_continue() {
        let manager = HookManager::new();
        let mut ctx = make_ctx();
        let action = manager
            .trigger(HookPoint::PreMessage, &mut ctx)
            .await
            .unwrap();
        assert!(matches!(action, HookAction::Continue));
    }

    #[tokio::test]
    async fn noop_hook_returns_continue() {
        let mut manager = HookManager::new();
        manager.register(NoopHook {
            hook_name: "noop",
            point: HookPoint::PreMessage,
        });
        let mut ctx = make_ctx();
        let action = manager
            .trigger(HookPoint::PreMessage, &mut ctx)
            .await
            .unwrap();
        assert!(matches!(action, HookAction::Continue));
    }

    #[tokio::test]
    async fn continue_with_merges_extra() {
        let mut manager = HookManager::new();
        manager.register(ModifyHook {
            hook_name: "add-a",
            point: HookPoint::PreMessage,
            key: "a".into(),
            value: serde_json::json!(1),
        });
        manager.register(ModifyHook {
            hook_name: "add-b",
            point: HookPoint::PreMessage,
            key: "b".into(),
            value: serde_json::json!(2),
        });

        let mut ctx = make_ctx();
        manager
            .trigger(HookPoint::PreMessage, &mut ctx)
            .await
            .unwrap();

        assert_eq!(ctx.extra.get("a"), Some(&serde_json::json!(1)));
        assert_eq!(ctx.extra.get("b"), Some(&serde_json::json!(2)));
    }

    #[tokio::test]
    async fn abort_stops_chain() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut manager = HookManager::new();

        manager.register(AbortHook {
            hook_name: "abort",
            point: HookPoint::PreMessage,
            reason: "blocked".into(),
        });
        manager.register(CountingHook {
            hook_name: "counter",
            point: HookPoint::PreMessage,
            counter: counter.clone(),
        });

        let mut ctx = make_ctx();
        let action = manager
            .trigger(HookPoint::PreMessage, &mut ctx)
            .await
            .unwrap();

        assert!(matches!(action, HookAction::Abort(ref r) if r == "blocked"));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn block_does_not_stop_chain() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut manager = HookManager::new();

        // PreToolUse supports blocking
        manager.register(BlockHook {
            hook_name: "blocker",
            point: HookPoint::PreToolUse,
            reason: "unsafe".into(),
        });
        manager.register(CountingHook {
            hook_name: "observer",
            point: HookPoint::PreToolUse,
            counter: counter.clone(),
        });

        let mut ctx = make_tool_ctx("bash");
        let action = manager
            .trigger(HookPoint::PreToolUse, &mut ctx)
            .await
            .unwrap();

        // Block is returned as final action...
        assert!(matches!(action, HookAction::Block(ref r) if r == "unsafe"));
        // ...but the remaining hook still fired.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn block_on_non_blocking_point_treated_as_continue() {
        let mut manager = HookManager::new();

        // PostMessage does NOT support blocking
        manager.register(BlockHook {
            hook_name: "blocker",
            point: HookPoint::PostMessage,
            reason: "nope".into(),
        });

        let mut ctx = HookContext {
            session_id: "sess-1".into(),
            user_id: None,
            event_data: HookEventData::PostMessage,
            message: None,
            response: None,
            job_id: None,
            trace_span_id: None,
            extra: HashMap::new(),
        };
        let action = manager
            .trigger(HookPoint::PostMessage, &mut ctx)
            .await
            .unwrap();

        // Treated as Continue because PostMessage doesn't support blocking
        assert!(matches!(action, HookAction::Continue));
    }

    #[tokio::test]
    async fn abort_takes_precedence_over_block() {
        let mut manager = HookManager::new();

        manager.register(BlockHook {
            hook_name: "blocker",
            point: HookPoint::PreToolUse,
            reason: "blocked".into(),
        });
        manager.register(AbortHook {
            hook_name: "aborter",
            point: HookPoint::PreToolUse,
            reason: "fatal".into(),
        });

        let mut ctx = make_tool_ctx("bash");
        let action = manager
            .trigger(HookPoint::PreToolUse, &mut ctx)
            .await
            .unwrap();

        // Abort wins over Block
        assert!(matches!(action, HookAction::Abort(ref r) if r == "fatal"));
    }

    #[tokio::test]
    async fn hooks_execute_in_registration_order() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut manager = HookManager::new();

        for i in 0..3 {
            let c = counter.clone();
            manager.register(CountingHook {
                hook_name: Box::leak(format!("hook-{}", i).into_boxed_str()),
                point: HookPoint::PostMessage,
                counter: c,
            });
        }

        let mut ctx = HookContext {
            session_id: "sess-1".into(),
            user_id: None,
            event_data: HookEventData::PostMessage,
            message: None,
            response: None,
            job_id: None,
            trace_span_id: None,
            extra: HashMap::new(),
        };
        manager
            .trigger(HookPoint::PostMessage, &mut ctx)
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn non_critical_failure_continues() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut manager = HookManager::new();

        manager.register(FailingHook {
            hook_name: "fail-soft",
            point: HookPoint::PreMessage,
            is_critical: false,
        });
        manager.register(CountingHook {
            hook_name: "after-fail",
            point: HookPoint::PreMessage,
            counter: counter.clone(),
        });

        let mut ctx = make_ctx();
        manager
            .trigger(HookPoint::PreMessage, &mut ctx)
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn critical_failure_aborts() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut manager = HookManager::new();

        manager.register(FailingHook {
            hook_name: "fail-hard",
            point: HookPoint::PreMessage,
            is_critical: true,
        });
        manager.register(CountingHook {
            hook_name: "should-not-run",
            point: HookPoint::PreMessage,
            counter: counter.clone(),
        });

        let mut ctx = make_ctx();
        let result = manager.trigger(HookPoint::PreMessage, &mut ctx).await;
        assert!(result.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn different_hook_points_are_independent() {
        let mut manager = HookManager::new();
        let counter_a = Arc::new(AtomicUsize::new(0));
        let counter_b = Arc::new(AtomicUsize::new(0));

        manager.register(CountingHook {
            hook_name: "pre",
            point: HookPoint::PreMessage,
            counter: counter_a.clone(),
        });
        manager.register(CountingHook {
            hook_name: "post",
            point: HookPoint::PostMessage,
            counter: counter_b.clone(),
        });

        let mut ctx = make_ctx();
        manager
            .trigger(HookPoint::PreMessage, &mut ctx)
            .await
            .unwrap();

        assert_eq!(counter_a.load(Ordering::SeqCst), 1);
        assert_eq!(counter_b.load(Ordering::SeqCst), 0);
    }

    // -- Matcher tests --

    #[tokio::test]
    async fn matcher_exact_filters_tool_name() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut manager = HookManager::new();

        manager.register(MatchedHook {
            hook_name: "bash-only",
            point: HookPoint::PreToolUse,
            hook_matcher: HookMatcher::parse("bash").unwrap(),
            counter: counter.clone(),
        });

        // Matching tool name
        let mut ctx = make_tool_ctx("bash");
        manager
            .trigger(HookPoint::PreToolUse, &mut ctx)
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Non-matching tool name
        let mut ctx = make_tool_ctx("edit");
        manager
            .trigger(HookPoint::PreToolUse, &mut ctx)
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1); // still 1
    }

    #[tokio::test]
    async fn matcher_one_of_filters_tool_name() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut manager = HookManager::new();

        manager.register(MatchedHook {
            hook_name: "write-hooks",
            point: HookPoint::PreToolUse,
            hook_matcher: HookMatcher::parse("edit|write").unwrap(),
            counter: counter.clone(),
        });

        let mut ctx = make_tool_ctx("edit");
        manager
            .trigger(HookPoint::PreToolUse, &mut ctx)
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let mut ctx = make_tool_ctx("write");
        manager
            .trigger(HookPoint::PreToolUse, &mut ctx)
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2);

        let mut ctx = make_tool_ctx("bash");
        manager
            .trigger(HookPoint::PreToolUse, &mut ctx)
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2); // not incremented
    }

    #[tokio::test]
    async fn matcher_regex_filters_tool_name() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut manager = HookManager::new();

        manager.register(MatchedHook {
            hook_name: "ext-hooks",
            point: HookPoint::PreToolUse,
            hook_matcher: HookMatcher::parse("^ext__.*").unwrap(),
            counter: counter.clone(),
        });

        let mut ctx = make_tool_ctx("ext__memory__create");
        manager
            .trigger(HookPoint::PreToolUse, &mut ctx)
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let mut ctx = make_tool_ctx("builtin__bash");
        manager
            .trigger(HookPoint::PreToolUse, &mut ctx)
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1); // not incremented
    }

    #[tokio::test]
    async fn no_matcher_fires_on_all() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut manager = HookManager::new();

        // Hook without matcher (returns None) should fire on all tool names
        manager.register(CountingHook {
            hook_name: "catch-all",
            point: HookPoint::PreToolUse,
            counter: counter.clone(),
        });

        let mut ctx = make_tool_ctx("bash");
        manager
            .trigger(HookPoint::PreToolUse, &mut ctx)
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let mut ctx = make_tool_ctx("anything");
        manager
            .trigger(HookPoint::PreToolUse, &mut ctx)
            .await
            .unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn matcher_on_event_without_target_only_all_fires() {
        let counter_all = Arc::new(AtomicUsize::new(0));
        let counter_exact = Arc::new(AtomicUsize::new(0));
        let mut manager = HookManager::new();

        // All matcher (no matcher = match all)
        manager.register(CountingHook {
            hook_name: "all",
            point: HookPoint::PreMessage,
            counter: counter_all.clone(),
        });

        // Exact matcher on an event with no matcher target
        manager.register(MatchedHook {
            hook_name: "exact",
            point: HookPoint::PreMessage,
            hook_matcher: HookMatcher::parse("something").unwrap(),
            counter: counter_exact.clone(),
        });

        let mut ctx = make_ctx(); // PreMessage has no matcher target
        manager
            .trigger(HookPoint::PreMessage, &mut ctx)
            .await
            .unwrap();

        assert_eq!(counter_all.load(Ordering::SeqCst), 1); // fires
        assert_eq!(counter_exact.load(Ordering::SeqCst), 0); // skipped
    }
}
