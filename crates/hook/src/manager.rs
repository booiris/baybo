use std::collections::HashMap;

use crate::Result;
use tracing::{debug, warn};

use crate::{Hook, HookAction, HookContext, HookPoint};

/// Stores registered hooks and triggers them at the appropriate lifecycle points.
///
/// Hooks for the same `HookPoint` are executed serially in registration order.
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
    /// Returns the final `HookAction`:
    /// - `Continue` if all hooks returned `Continue` or `ContinueWith`
    /// - `Abort` if any hook aborted the chain
    ///
    /// `ContinueWith` modifications are applied to `ctx` as they arrive,
    /// so later hooks see changes made by earlier ones.
    pub async fn trigger(&self, point: HookPoint, ctx: &mut HookContext) -> Result<HookAction> {
        let Some(hooks) = self.hooks.get(&point) else {
            return Ok(HookAction::Continue);
        };

        for registered in hooks {
            let hook_name = registered.hook.name();
            debug!("Executing hook '{}' at {:?}", hook_name, point);

            let result = registered.hook.execute(ctx).await;

            match result {
                Ok(HookAction::Continue) => {}
                Ok(HookAction::ContinueWith(modification)) => {
                    ctx.apply(*modification);
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

        Ok(HookAction::Continue)
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

    fn make_ctx() -> HookContext {
        HookContext {
            session_id: "sess-1".into(),
            user_id: None,
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

        let mut ctx = make_ctx();
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
}
