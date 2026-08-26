//! Per-turn detection of tool activity that has stopped making progress.
//!
//! File mutations retain their state-transition history so revisits and
//! cancelled-out edits can be recognized. Every other tool retains only the
//! current consecutive error signature. The two detectors stay separate
//! internally because their evidence and reset rules differ; this module
//! provides the one turn lifecycle and verdict surface the agent loop needs.
//!
//! # Why not a content check
//!
//! The shape this exists to catch is an oscillation: remove a config entry,
//! restore it, add a no-op argument, revert that — five edits, zero net change,
//! and the model never noticed because nothing told it. A "did the bytes
//! change?" test cannot see that. `Edit` already rejects `old_string ==
//! new_string` (`baybo_tools::builtin::edit`), so **every** applied edit
//! changes the file; and [`baybo_model::FileFingerprint`] carries an mtime, so
//! it is monotonic even when the content returns to a prior state.
//!
//! What identifies the shape is the *sequence of state transitions*. An `Edit`
//! names both endpoints of its own transition — `old_string` is the state it
//! consumes, `new_string` the state it leaves — so an edit whose `new_string`
//! reproduces an earlier edit's `old_string` has undone that earlier edit. A
//! `Write` names only its result, which is the whole file, so its result is
//! compared against prior results on the same path.
//!
//! Only the two hashes are retained, never the payloads: the ledger is a
//! same-turn scratchpad, and holding edit bodies for a turn that rewrites a
//! large file would be a real memory cost for no gain.

use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use baybo_tools::ToolOutput;

/// Distinct files tracked in one turn, least-recently-touched evicted first.
///
/// Sized against the failure mode, not against memory: a turn that sweeps a
/// crate and *then* starts churning one file is exactly the case worth
/// catching, and a small cap would have evicted that file's history before the
/// churn began. A `TargetHistory` is 64 B plus its path and attempt window, so
/// even a pathological turn that fills every slot costs a few hundred KB
/// against an LLM context measured in megabytes.
const MAX_TRACKED_TARGETS: usize = 128;

/// Attempts retained per file — the detection window.
///
/// Iterating on one file dozens of times in a turn is ordinary work (write,
/// test, fix, fix again), and the window has to outlast that for a later
/// revisit to still find the state it returned to. Each `Attempt` is 32 B, so
/// the whole window is ~2 KB per file.
const MAX_ATTEMPTS_PER_TARGET: usize = 64;

/// Churn signals (repeats + revisits, accumulated over the turn) a single file
/// must produce before anything is said about it.
///
/// One is not evidence of a loop: reverting an edit you just made is a normal
/// way to explore, and a lone A→B→A is indistinguishable from a flag toggled
/// on to test and back off. Three is a file the turn is visibly failing to
/// move. The incident this was built from reached three on its fifth edit —
/// one edit before the user asked what was going on — so the threshold buys a
/// large drop in false positives for almost no delay on the real thing.
const CHURN_SIGNALS_BEFORE_REPORT: usize = 3;

/// Consecutive successful, advancing edits to one file that clear its
/// accumulated churn signals.
///
/// Must stay above one, or the detector loses its own founding case: the
/// incident had a single genuinely-new edit (`--generation=2`) sitting between
/// its second and third signals, and a one-edit decay would have wiped the
/// count right before the signal that convicted it. Three is a file that is
/// demonstrably being moved rather than shuffled.
const CLEAN_EDITS_THAT_CLEAR_SIGNALS: usize = 3;

/// Consecutive denied/failed attempts on one file that count as futile.
///
/// Unlike the two caps above this is a *sensitivity* threshold, not a memory
/// bound, and it does not want to grow: two failures is an ordinary retry
/// after a fixable error (a stale `old_string`, a missing parent dir), three
/// in a row is a pattern. Raising it only means more wasted attempts before
/// anyone says anything.
const FUTILE_STREAK_THRESHOLD: usize = 3;

/// Identical consecutive failures from one non-file tool before the model is
/// told it is retrying the same cause.
const REPEATED_TOOL_FAILURE_THRESHOLD: usize = 3;

/// One mutation's endpoints, as hashes.
///
/// `from` is `None` when the tool did not name the state it consumed — a
/// `Write` replaces the file wholesale without quoting what was there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StateTransition {
    from: Option<u64>,
    to: u64,
}

/// Whether the attempt actually changed the file. Denied and failed collapse
/// into one variant on purpose: a `/stop` mid-turn surfaces as
/// [`baybo_tools::ToolError::Denied`], so "the user refused" and "the tool
/// errored" are not reliably distinguishable here, and the ledger treats both
/// only as "the file did not move".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptOutcome {
    Applied,
    Rejected,
}

#[derive(Debug, Clone, Copy)]
struct Attempt {
    transition: StateTransition,
    outcome: AttemptOutcome,
}

/// What either progress detector concluded. Each verdict carries the count
/// that justifies it so the injected observation can be specific rather than
/// scolding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProgressVerdict {
    /// The exact same mutation was submitted again — including the case where
    /// the first submission was denied and the retry was identical.
    AttemptRepeated { path: PathBuf, attempts: usize },
    /// The file is back in a state it already held this turn: the edits
    /// cancelled out.
    StateRevisited { path: PathBuf, edits: usize },
    /// A run of attempts on one file that all failed or were refused.
    Futile { path: PathBuf, streak: usize },
    /// The same non-file tool returned the exact same error consecutively.
    RepeatedToolFailure { tool_name: String, attempts: usize },
}

struct TargetHistory {
    path: PathBuf,
    attempts: VecDeque<Attempt>,
    /// How many times this file has produced a churn signal (a repeat or a
    /// revisit) this turn. Nothing is said until it reaches
    /// [`CHURN_SIGNALS_BEFORE_REPORT`].
    ///
    /// Cleared by [`CLEAN_EDITS_THAT_CLEAR_SIGNALS`] consecutive advancing
    /// edits, at the turn boundary ([`TurnProgressMonitor::clear`]), and by
    /// eviction dropping the whole entry. A scattering of signals early in a
    /// long turn must not still be waiting, hundreds of iterations later, to
    /// convict an unrelated third.
    churn_signals: usize,
    /// Consecutive successful edits that produced no churn signal. Reset by
    /// any signal and by any refusal — the decay wants a run of edits that
    /// actually moved the file.
    clean_streak: usize,
    /// Whether this file has already produced an observation this turn. One
    /// per file per turn: the point is to make the model look, and repeating
    /// the same line every iteration would just become noise it learns to
    /// scroll past.
    reported: bool,
}

/// A repeat or a revisit, detected but not yet worth saying anything about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChurnSignal {
    Repeated,
    Revisited,
}

/// State-transition history for file mutations.
#[derive(Default)]
struct FileMutationLedger {
    targets: VecDeque<TargetHistory>,
}

impl FileMutationLedger {
    fn clear(&mut self) {
        self.targets.clear();
    }

    /// Record one file-mutating tool call and report what it revealed.
    ///
    /// Returns `None` for every call that is not a file mutation, for a file
    /// that has already been reported this turn, and — the common case — for a
    /// call that made ordinary progress.
    fn record(
        &mut self,
        tool_name: &str,
        arguments: &serde_json::Value,
        applied: bool,
    ) -> Option<ProgressVerdict> {
        if !baybo_tools::FILE_WRITING_TOOLS.contains(&tool_name) {
            return None;
        }
        let path = PathBuf::from(
            arguments
                .get(baybo_tools::TOOL_FILE_PATH_ARG)
                .and_then(|v| v.as_str())?,
        );
        let transition = transition_for(tool_name, arguments)?;
        let outcome = if applied {
            AttemptOutcome::Applied
        } else {
            AttemptOutcome::Rejected
        };

        let index = self.target_index(&path);
        let history = self.targets.get_mut(index)?;

        // Signals accumulate across the turn rather than needing to be
        // consecutive — a file that churns, works a little, then churns again
        // is still churning. But a *sustained* run of successful, advancing
        // edits says the file is being moved after all, and clears the slate:
        // otherwise two stray signals in iteration 3 would still be sitting
        // there in iteration 400, waiting to convict an unrelated third.
        let signal = churn_signal_for(history, transition, outcome);
        if signal.is_some() {
            history.churn_signals += 1;
            history.clean_streak = 0;
        } else if outcome == AttemptOutcome::Applied {
            history.clean_streak += 1;
            if history.clean_streak >= CLEAN_EDITS_THAT_CLEAR_SIGNALS {
                history.churn_signals = 0;
                history.clean_streak = 0;
            }
        } else {
            // A refusal is not progress, so it does not earn decay — but it
            // does break the run of successful edits.
            history.clean_streak = 0;
        }
        let attempts = history.attempts.len() + 1;
        let verdict = (!history.reported)
            .then(|| verdict_for(history, signal, outcome, attempts))
            .flatten();

        if history.attempts.len() == MAX_ATTEMPTS_PER_TARGET {
            history.attempts.pop_front();
        }
        history.attempts.push_back(Attempt {
            transition,
            outcome,
        });
        if verdict.is_some() {
            history.reported = true;
        }
        verdict
    }

    /// Index of `path`'s history, inserting an empty one when this is the
    /// file's first mutation this turn.
    ///
    /// Touching a file promotes it to the back, so eviction is
    /// least-recently-touched rather than first-seen. Insertion order would
    /// evict precisely the wrong entry: a file edited early and churned
    /// throughout is the one whose history matters, and it is also the one a
    /// FIFO drops as soon as `MAX_TRACKED_TARGETS` newer files appear.
    fn target_index(&mut self, path: &Path) -> usize {
        if let Some(i) = self.targets.iter().position(|t| t.path == path) {
            if let Some(history) = self.targets.remove(i) {
                self.targets.push_back(history);
            }
            return self.targets.len() - 1;
        }
        if self.targets.len() == MAX_TRACKED_TARGETS {
            self.targets.pop_front();
        }
        self.targets.push_back(TargetHistory {
            path: path.to_path_buf(),
            attempts: VecDeque::new(),
            churn_signals: 0,
            clean_streak: 0,
            reported: false,
        });
        self.targets.len() - 1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FailureSignature {
    tool_name: String,
    error_hash: u64,
}

#[derive(Debug)]
struct FailureStreak {
    signature: FailureSignature,
    attempts: usize,
    reported: bool,
}

#[derive(Debug, Default)]
struct ToolFailureDetector {
    streak: Option<FailureStreak>,
}

impl ToolFailureDetector {
    fn clear(&mut self) {
        self.streak = None;
    }

    fn record(
        &mut self,
        tool_name: &str,
        output: &anyhow::Result<ToolOutput>,
    ) -> Option<ProgressVerdict> {
        let Some(error_hash) = error_hash(output) else {
            self.clear();
            return None;
        };
        let signature = FailureSignature {
            tool_name: tool_name.to_owned(),
            error_hash,
        };

        match &mut self.streak {
            Some(streak) if streak.signature == signature => {
                streak.attempts += 1;
                if streak.attempts >= REPEATED_TOOL_FAILURE_THRESHOLD && !streak.reported {
                    streak.reported = true;
                    return Some(ProgressVerdict::RepeatedToolFailure {
                        tool_name: tool_name.to_owned(),
                        attempts: streak.attempts,
                    });
                }
            }
            _ => {
                self.streak = Some(FailureStreak {
                    signature,
                    attempts: 1,
                    reported: false,
                });
            }
        }
        None
    }
}

/// The one per-turn progress monitor owned by the agent loop.
///
/// File-writing tools route to the transition detector; every other tool
/// routes to the consecutive-error detector. A file call also breaks a
/// non-file failure streak, matching the ordinary "another tool was tried"
/// reset rule.
#[derive(Default)]
pub(crate) struct TurnProgressMonitor {
    files: FileMutationLedger,
    failures: ToolFailureDetector,
}

impl TurnProgressMonitor {
    pub(crate) fn clear(&mut self) {
        self.files.clear();
        self.failures.clear();
    }

    pub(crate) fn record(
        &mut self,
        tool_name: &str,
        arguments: &serde_json::Value,
        output: &anyhow::Result<ToolOutput>,
    ) -> Option<ProgressVerdict> {
        if baybo_tools::FILE_WRITING_TOOLS.contains(&tool_name) {
            self.failures.clear();
            return self
                .files
                .record(tool_name, arguments, tool_call_succeeded(output));
        }
        self.failures.record(tool_name, output)
    }
}

/// Whether a tool result represents an applied/successful call.
///
/// `ToolOutput::Error` is an in-band failure and therefore does not count as
/// success even though it rides inside `Ok`.
pub(crate) fn tool_call_succeeded(output: &anyhow::Result<ToolOutput>) -> bool {
    matches!(output, Ok(value) if !matches!(value, ToolOutput::Error(_)))
}

fn error_hash(output: &anyhow::Result<ToolOutput>) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    match output {
        Ok(ToolOutput::Error(reason)) => reason.hash(&mut hasher),
        Err(error) => error.to_string().hash(&mut hasher),
        Ok(_) => return None,
    }
    Some(hasher.finish())
}

/// Whether this attempt repeats or undoes something the file already saw.
/// Ordered most-specific first: an exact resubmission is a sharper thing to
/// tell the model than the revisit it also technically is.
fn churn_signal_for(
    history: &TargetHistory,
    transition: StateTransition,
    outcome: AttemptOutcome,
) -> Option<ChurnSignal> {
    if history
        .attempts
        .iter()
        .any(|a| a.transition == transition && a.transition.from.is_some())
    {
        return Some(ChurnSignal::Repeated);
    }

    // Only applied attempts define states the file actually held; a denied
    // edit's endpoints never existed on disk, so matching against them would
    // invent a revisit out of an attempt that changed nothing.
    if outcome == AttemptOutcome::Applied
        && history
            .attempts
            .iter()
            .filter(|a| a.outcome == AttemptOutcome::Applied)
            .any(|a| a.transition.from == Some(transition.to) || a.transition.to == transition.to)
    {
        return Some(ChurnSignal::Revisited);
    }

    None
}

/// Turn the state of one file into something worth telling the model, or
/// nothing.
///
/// `history.churn_signals` already counts this attempt's signal, so the
/// threshold is checked against the running total rather than the incoming
/// one. A file below the threshold can still report `Futile`: that rule counts
/// its own consecutive refusals and is unrelated to churn.
fn verdict_for(
    history: &TargetHistory,
    signal: Option<ChurnSignal>,
    outcome: AttemptOutcome,
    attempts: usize,
) -> Option<ProgressVerdict> {
    let path = history.path.clone();
    if signal.is_some() && history.churn_signals >= CHURN_SIGNALS_BEFORE_REPORT {
        return Some(match signal {
            Some(ChurnSignal::Repeated) => ProgressVerdict::AttemptRepeated { path, attempts },
            _ => ProgressVerdict::StateRevisited {
                path,
                edits: attempts,
            },
        });
    }

    if outcome == AttemptOutcome::Rejected {
        let streak = 1 + history
            .attempts
            .iter()
            .rev()
            .take_while(|a| a.outcome == AttemptOutcome::Rejected)
            .count();
        if streak >= FUTILE_STREAK_THRESHOLD {
            return Some(ProgressVerdict::Futile {
                path: history.path.clone(),
                streak,
            });
        }
    }

    None
}

/// The endpoints a mutation names in its own arguments. `None` when the call
/// is malformed enough that there is nothing to compare — a missing body is
/// the tool's error to report, not the ledger's.
fn transition_for(tool_name: &str, arguments: &serde_json::Value) -> Option<StateTransition> {
    let arg = |key: &str| arguments.get(key).and_then(|v| v.as_str());
    if tool_name == baybo_tools::WRITE_TOOL_NAME {
        return Some(StateTransition {
            from: None,
            to: hash_of(arg(baybo_tools::TOOL_CONTENT_ARG)?),
        });
    }
    Some(StateTransition {
        from: Some(hash_of(arg(baybo_tools::EDIT_OLD_STRING_ARG)?)),
        to: hash_of(arg(baybo_tools::EDIT_NEW_STRING_ARG)?),
    })
}

/// Identity, not integrity: the question is only "same string as before?", and
/// the other party is the model itself rather than an attacker, so the cheap
/// hasher is the right trade (as in `baybo_tools::mcp::reconciler`).
fn hash_of(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MCP: &str = "/home/u/.baybo/config/.mcp.json";
    const SERVERS_FULL: &str = "  \"servers\": [\n    { \"name\": \"netdata-alerts\" }\n  ]";
    const SERVERS_EMPTY: &str = "  \"servers\": []";

    fn edit(path: &str, old: &str, new: &str) -> serde_json::Value {
        json!({ "file_path": path, "old_string": old, "new_string": new })
    }

    fn write(path: &str, content: &str) -> serde_json::Value {
        json!({ "file_path": path, "content": content })
    }

    const ARGS_PLAIN: &str = "\"--script\",\n\"mcp_server.py\"";
    const ARGS_GEN2: &str = "\"--script\",\n\"mcp_server.py\",\n\"--generation=2\"";

    /// The incident, replayed with its real edit shapes: remove the server
    /// (denied), resubmit the identical edit (applied), revert it, add a no-op
    /// `--generation=2`, revert that. Session
    /// cddcfcdb-c5f8-43fc-bb83-01385d0a7b31, ordinals #143/#155/#161/#173/#185.
    ///
    /// Three churn signals accumulate across those five edits, and the third
    /// lands on the last one — one edit before the user asked what was going
    /// on. Nothing is said before that.
    #[test]
    fn the_incident_reports_on_its_third_churn_signal() {
        let mut ledger = FileMutationLedger::default();
        // #143 — denied. Nothing to compare against yet.
        assert_eq!(
            ledger.record("Edit", &edit(MCP, SERVERS_FULL, SERVERS_EMPTY), false),
            None
        );
        // #155 — byte-identical resubmission, this time approved. Signal 1.
        assert_eq!(
            ledger.record("Edit", &edit(MCP, SERVERS_FULL, SERVERS_EMPTY), true),
            None,
            "one signal is not a loop"
        );
        // #161 — puts the server back: the file is where it started. Signal 2.
        assert_eq!(
            ledger.record("Edit", &edit(MCP, SERVERS_EMPTY, SERVERS_FULL), true),
            None,
            "two is still inside the noise floor"
        );
        // #173 — a genuinely new edit: append `--generation=2`. No signal.
        assert_eq!(
            ledger.record("Edit", &edit(MCP, ARGS_PLAIN, ARGS_GEN2), true),
            None
        );
        // #185 — revert it. Signal 3, and the turn finally hears about it.
        assert_eq!(
            ledger.record("Edit", &edit(MCP, ARGS_GEN2, ARGS_PLAIN), true),
            Some(ProgressVerdict::StateRevisited {
                path: PathBuf::from(MCP),
                edits: 5
            })
        );
    }

    /// The whole point of the threshold: undoing an edit you just made is a
    /// normal way to explore, and a lone A→B→A is indistinguishable from a
    /// flag toggled on to test and back off.
    #[test]
    fn a_single_deliberate_oscillation_says_nothing() {
        let mut ledger = FileMutationLedger::default();
        assert_eq!(ledger.record("Edit", &edit(MCP, "off", "on"), true), None);
        assert_eq!(ledger.record("Edit", &edit(MCP, "on", "off"), true), None);
    }

    #[test]
    fn an_exact_resubmission_reports_as_a_repeat_once_over_threshold() {
        let mut ledger = FileMutationLedger::default();
        // Two signals of churn to get to the threshold's edge…
        ledger.record("Edit", &edit(MCP, "a", "b"), true);
        ledger.record("Edit", &edit(MCP, "b", "a"), true); // signal 1
        ledger.record("Edit", &edit(MCP, "a", "b"), true); // signal 2
        // …then an exact resubmission of an earlier transition. The verdict
        // names what just happened, not merely that the file is churning.
        assert!(matches!(
            ledger.record("Edit", &edit(MCP, "b", "a"), true),
            Some(ProgressVerdict::AttemptRepeated { .. })
        ));
    }

    #[test]
    fn only_one_observation_per_file_per_turn() {
        let mut ledger = FileMutationLedger::default();
        ledger.record("Edit", &edit(MCP, "a", "b"), true);
        ledger.record("Edit", &edit(MCP, "b", "a"), true); // signal 1
        ledger.record("Edit", &edit(MCP, "a", "b"), true); // signal 2
        assert!(
            ledger.record("Edit", &edit(MCP, "b", "a"), true).is_some(),
            "signal 3 reports"
        );
        // Every later signal on the same file is silent: the model has been
        // told, and repeating it would just become noise.
        assert_eq!(ledger.record("Edit", &edit(MCP, "a", "b"), true), None);
        assert_eq!(ledger.record("Edit", &edit(MCP, "b", "a"), true), None);
    }

    /// Eviction restarts the count, because it drops the whole entry. This is
    /// a consequence of the LRU bound rather than a rule of its own, but it is
    /// the only way a tracked file's signals go back to zero mid-turn, so it
    /// is pinned here rather than left to be discovered.
    ///
    /// It is also defensible: a file that has gone untouched while a hundred
    /// others were edited is not in a tight loop any more.
    #[test]
    fn eviction_restarts_a_files_signal_count() {
        let mut ledger = FileMutationLedger::default();
        ledger.record("Edit", &edit(MCP, "a", "b"), true);
        ledger.record("Edit", &edit(MCP, "b", "a"), true); // signal 1
        ledger.record("Edit", &edit(MCP, "a", "b"), true); // signal 2

        // Push it out without touching it again — LRU keeps a *touched* file
        // alive, so this has to leave it alone.
        for i in 0..MAX_TRACKED_TARGETS {
            ledger.record("Edit", &edit(&format!("/swept{i}"), "x", "y"), true);
        }

        // A fresh history: the first edit has nothing to match, the second is
        // signal 1 of a new count — not signal 3 of the old one.
        assert_eq!(ledger.record("Edit", &edit(MCP, "p", "q"), true), None);
        assert_eq!(ledger.record("Edit", &edit(MCP, "q", "p"), true), None);
    }

    /// A sustained run of advancing edits says the file is being moved after
    /// all, and wipes the accumulated signals. Without this, two stray signals
    /// in iteration 3 would still be sitting there in iteration 400, waiting
    /// to convict an unrelated third.
    #[test]
    fn a_sustained_run_of_progress_clears_the_signals() {
        let mut ledger = FileMutationLedger::default();
        ledger.record("Edit", &edit(MCP, "a", "b"), true);
        ledger.record("Edit", &edit(MCP, "b", "a"), true); // signal 1
        ledger.record("Edit", &edit(MCP, "a", "b"), true); // signal 2

        // Three consecutive advancing edits — the file is going somewhere.
        for (old, new) in [("b", "n1"), ("n1", "n2"), ("n2", "n3")] {
            assert_eq!(ledger.record("Edit", &edit(MCP, old, new), true), None);
        }

        // Back to zero: this revisit is signal 1 of a fresh count, not 3.
        assert_eq!(ledger.record("Edit", &edit(MCP, "n3", "n2"), true), None);
    }

    /// A refusal is not progress. A run of successful edits broken by one
    /// denial has to start over, or a model alternating "try, get refused,
    /// edit something else" would decay its way out of ever being noticed.
    #[test]
    fn a_refusal_breaks_the_run_of_progress() {
        let mut ledger = FileMutationLedger::default();
        ledger.record("Edit", &edit(MCP, "a", "b"), true);
        ledger.record("Edit", &edit(MCP, "b", "a"), true); // signal 1
        ledger.record("Edit", &edit(MCP, "a", "b"), true); // signal 2

        ledger.record("Edit", &edit(MCP, "b", "n1"), true); // clean 1
        ledger.record("Edit", &edit(MCP, "n1", "n2"), false); // refused → run broken
        ledger.record("Edit", &edit(MCP, "b", "n3"), true); // clean 1 again

        // The signals were never cleared, so the next revisit is the third.
        assert!(
            ledger.record("Edit", &edit(MCP, "n3", "b"), true).is_some(),
            "a broken run must not have decayed the count"
        );
    }

    /// Signals need not be consecutive: a *short* burst of work between them
    /// is still one file the turn is failing to move.
    #[test]
    fn progress_between_signals_does_not_reset_the_count() {
        let mut ledger = FileMutationLedger::default();
        ledger.record("Edit", &edit(MCP, "a", "b"), true);
        ledger.record("Edit", &edit(MCP, "b", "a"), true); // signal 1
        ledger.record("Edit", &edit(MCP, "a", "z1"), true); // real progress
        ledger.record("Edit", &edit(MCP, "z1", "z2"), true); // real progress
        ledger.record("Edit", &edit(MCP, "z2", "z1"), true); // signal 2
        ledger.record("Edit", &edit(MCP, "z1", "z3"), true); // real progress
        assert!(
            ledger
                .record("Edit", &edit(MCP, "z3", "z1"), true)
                .is_some(),
            "signal 3 reports even with progress interleaved"
        );
    }

    #[test]
    fn a_denied_attempts_endpoints_are_not_states_the_file_held() {
        let mut ledger = FileMutationLedger::default();
        // Denied: the file never became "b".
        ledger.record("Edit", &edit(MCP, "a", "b"), false);
        // A different edit landing on "b" is genuine progress, not a revisit.
        assert_eq!(ledger.record("Edit", &edit(MCP, "x", "b"), true), None);
    }

    #[test]
    fn ordinary_iterative_repair_is_silent() {
        // The common shape this must not fire on: write, test, fix, fix again.
        // Every `new_string` is new, so no state ever recurs.
        let mut ledger = FileMutationLedger::default();
        for (old, new) in [("v0", "v1"), ("v1", "v2"), ("v2", "v3"), ("v3", "v4")] {
            assert_eq!(
                ledger.record("Edit", &edit(MCP, old, new), true),
                None,
                "{old}->{new}"
            );
        }
    }

    #[test]
    fn a_whole_file_write_revisits_by_its_result() {
        // `Write` does not quote what it replaced, so the comparison is
        // result-to-result. A→B→A→B→A: three of those five land on a body the
        // file already had.
        let mut ledger = FileMutationLedger::default();
        for body in ["A", "B", "A", "B"] {
            assert_eq!(
                ledger.record("Write", &write(MCP, body), true),
                None,
                "{body}"
            );
        }
        assert!(matches!(
            ledger.record("Write", &write(MCP, "A"), true),
            Some(ProgressVerdict::StateRevisited { .. })
        ));
    }

    #[test]
    fn repeated_identical_writes_are_not_reported_as_a_repeat() {
        // Two `Write`s with the same body are indistinguishable from a
        // deliberate idempotent rewrite (a render step, a formatter), and
        // `from` is unknown for both — so they must never trip the exact-match
        // rule, however many of them there are. They still count as revisits.
        let mut ledger = FileMutationLedger::default();
        ledger.record("Write", &write(MCP, "A"), true);
        ledger.record("Write", &write(MCP, "A"), true);
        ledger.record("Write", &write(MCP, "A"), true);
        assert!(matches!(
            ledger.record("Write", &write(MCP, "A"), true),
            Some(ProgressVerdict::StateRevisited { .. })
        ));
    }

    #[test]
    fn three_consecutive_refusals_on_one_file_are_futile() {
        let mut ledger = FileMutationLedger::default();
        assert_eq!(ledger.record("Edit", &edit(MCP, "a", "b"), false), None);
        assert_eq!(ledger.record("Edit", &edit(MCP, "c", "d"), false), None);
        assert_eq!(
            ledger.record("Edit", &edit(MCP, "e", "f"), false),
            Some(ProgressVerdict::Futile {
                path: PathBuf::from(MCP),
                streak: 3
            })
        );
    }

    #[test]
    fn a_success_breaks_the_futile_streak() {
        let mut ledger = FileMutationLedger::default();
        ledger.record("Edit", &edit(MCP, "a", "b"), false);
        ledger.record("Edit", &edit(MCP, "c", "d"), false);
        ledger.record("Edit", &edit(MCP, "e", "f"), true);
        assert_eq!(ledger.record("Edit", &edit(MCP, "g", "h"), false), None);
    }

    #[test]
    fn files_are_tracked_independently() {
        let mut ledger = FileMutationLedger::default();
        ledger.record("Edit", &edit(MCP, "a", "b"), true);
        // The same transition on a DIFFERENT file is unrelated work.
        assert_eq!(ledger.record("Edit", &edit("/other", "a", "b"), true), None);
    }

    #[test]
    fn non_mutating_tools_and_malformed_arguments_are_ignored() {
        let mut ledger = FileMutationLedger::default();
        assert_eq!(ledger.record("Read", &edit(MCP, "a", "b"), true), None);
        assert_eq!(
            ledger.record("Bash", &json!({ "command": "ls" }), true),
            None
        );
        assert_eq!(
            ledger.record("Edit", &json!({ "file_path": MCP }), true),
            None
        );
        assert_eq!(
            ledger.record("Edit", &json!({ "old_string": "a" }), true),
            None
        );
    }

    #[test]
    fn clear_drops_every_turns_history() {
        let mut ledger = FileMutationLedger::default();
        ledger.record("Edit", &edit(MCP, "a", "b"), true);
        ledger.clear();
        // The revert is now the turn's first sighting of this file: the user
        // asking for it back is ordinary work.
        assert_eq!(ledger.record("Edit", &edit(MCP, "b", "a"), true), None);
    }

    #[test]
    fn a_wide_turn_evicts_the_oldest_target_rather_than_growing() {
        let mut ledger = FileMutationLedger::default();
        for i in 0..MAX_TRACKED_TARGETS + 4 {
            ledger.record("Edit", &edit(&format!("/f{i}"), "a", "b"), true);
        }
        assert_eq!(ledger.targets.len(), MAX_TRACKED_TARGETS);
        // `/f0` was evicted, so its revert reads as a first sighting.
        assert_eq!(ledger.record("Edit", &edit("/f0", "b", "a"), true), None);
    }

    /// The case insertion-order eviction got backwards: a file that is being
    /// churned throughout a wide turn is the one whose history matters, and a
    /// FIFO drops it precisely because it was seen first.
    #[test]
    fn a_file_touched_throughout_a_wide_sweep_survives_eviction() {
        let mut ledger = FileMutationLedger::default();
        // The state the file will eventually be dragged back to.
        assert_eq!(
            ledger.record("Edit", &edit("/churned", "s0", "s1"), true),
            None
        );

        // A sweep wide enough to evict every other slot, with the churned file
        // advanced along the way — each touch a genuine step, so nothing fires
        // and the promotion is the only thing keeping its history alive. The
        // touches stay well inside `MAX_ATTEMPTS_PER_TARGET` so the `s1` state
        // is still in the window at the end.
        let mut state = 1;
        for i in 0..MAX_TRACKED_TARGETS * 2 {
            ledger.record("Edit", &edit(&format!("/swept{i}"), "x", "y"), true);
            if i % 16 == 0 {
                let next = state + 1;
                assert_eq!(
                    ledger.record(
                        "Edit",
                        &edit("/churned", &format!("s{state}"), &format!("s{next}")),
                        true
                    ),
                    None,
                    "advancing the file is not churn"
                );
                state = next;
            }
        }

        // Now churn it. Signal 1 is the eviction-sensitive one: `s1` was
        // reached before the sweep, so it only matches if the file's history
        // survived. Had it been evicted, these three edits would look like a
        // fresh file and produce one signal between them, not three.
        assert_eq!(
            ledger.record("Edit", &edit("/churned", &format!("s{state}"), "s1"), true),
            None
        );
        assert_eq!(
            ledger.record("Edit", &edit("/churned", "s1", "s2"), true),
            None
        );
        assert!(matches!(
            ledger.record("Edit", &edit("/churned", "s2", "s1"), true),
            Some(ProgressVerdict::StateRevisited { .. })
        ));
    }

    #[test]
    fn the_attempt_window_is_bounded() {
        let mut ledger = FileMutationLedger::default();
        for i in 0..MAX_ATTEMPTS_PER_TARGET * 2 {
            ledger.record(
                "Edit",
                &edit(MCP, &format!("v{i}"), &format!("v{}", i + 1)),
                true,
            );
        }
        assert_eq!(ledger.targets[0].attempts.len(), MAX_ATTEMPTS_PER_TARGET);
    }

    fn failed(reason: &str) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::Error(reason.to_owned()))
    }

    #[test]
    fn monitor_reports_the_third_identical_non_file_failure() {
        let mut monitor = TurnProgressMonitor::default();
        let arguments = json!({ "title": "broken" });

        for _ in 0..2 {
            assert_eq!(
                monitor.record("IssueCreate", &arguments, &failed("invalid parent_id")),
                None
            );
        }
        assert_eq!(
            monitor.record("IssueCreate", &arguments, &failed("invalid parent_id")),
            Some(ProgressVerdict::RepeatedToolFailure {
                tool_name: "IssueCreate".to_owned(),
                attempts: 3,
            })
        );
    }

    #[test]
    fn monitor_resets_failure_streak_on_changed_outcome_tool_or_turn() {
        let mut monitor = TurnProgressMonitor::default();
        let arguments = json!({});

        monitor.record("IssueCreate", &arguments, &failed("same"));
        monitor.record("IssueCreate", &arguments, &failed("different"));
        monitor.record("IssueUpdate", &arguments, &failed("same"));
        assert_eq!(
            monitor.record(
                "IssueUpdate",
                &arguments,
                &Ok(ToolOutput::Text("ok".to_owned()))
            ),
            None
        );

        monitor.record("IssueCreate", &arguments, &failed("same"));
        monitor.record("IssueCreate", &arguments, &failed("same"));
        monitor.clear();
        assert_eq!(
            monitor.record("IssueCreate", &arguments, &failed("same")),
            None
        );
    }

    #[test]
    fn monitor_routes_file_tools_to_the_transition_detector() {
        let mut monitor = TurnProgressMonitor::default();
        let arguments = edit(MCP, "a", "b");

        assert_eq!(monitor.record("Edit", &arguments, &failed("denied")), None);
        assert_eq!(monitor.record("Edit", &arguments, &failed("denied")), None);
        assert!(matches!(
            monitor.record("Edit", &arguments, &failed("denied")),
            Some(ProgressVerdict::Futile { streak: 3, .. })
        ));
    }
}
