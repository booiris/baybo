import test from "node:test";
import assert from "node:assert/strict";

import {
  SESSION_EXPIRED_ERRCODE,
  _resetForTest,
  assertSessionActive,
  getRemainingPauseMs,
  isSessionPaused,
  pauseSession,
} from "../dist/api/session-guard.js";

function withFakeTime(fn) {
  const orig = Date.now;
  let now = 1_700_000_000_000;
  Date.now = () => now;
  try {
    fn({
      advance(ms) {
        now += ms;
      },
    });
  } finally {
    Date.now = orig;
  }
}

test("SESSION_EXPIRED_ERRCODE equals -14", () => {
  assert.equal(SESSION_EXPIRED_ERRCODE, -14);
});

test("pauseSession sets a one-hour cooldown", () => {
  _resetForTest();
  withFakeTime((clock) => {
    pauseSession("acct-1");
    assert.equal(isSessionPaused("acct-1"), true);
    const remaining = getRemainingPauseMs("acct-1");
    assert.ok(remaining > 0 && remaining <= 60 * 60 * 1000);
    clock.advance(30 * 60 * 1000);
    assert.equal(isSessionPaused("acct-1"), true);
    clock.advance(30 * 60 * 1000 + 1);
    assert.equal(isSessionPaused("acct-1"), false);
  });
});

test("assertSessionActive throws while paused, resolves after cooldown", () => {
  _resetForTest();
  withFakeTime((clock) => {
    pauseSession("acct-2");
    assert.throws(() => assertSessionActive("acct-2"), /paused/i);
    clock.advance(60 * 60 * 1000 + 1);
    // After cooldown, no throw.
    assertSessionActive("acct-2");
  });
});

test("pauseSession is scoped per accountId", () => {
  _resetForTest();
  withFakeTime(() => {
    pauseSession("a");
    assert.equal(isSessionPaused("a"), true);
    assert.equal(isSessionPaused("b"), false);
    assertSessionActive("b");
  });
});
