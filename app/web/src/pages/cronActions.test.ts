import { describe, expect, it } from 'vitest';
import createClient from 'openapi-fetch';

import type { AdminClient } from '../api/client';
import type { components, paths } from '../api/schema';
import {
  actOnCronJob,
  cronEditIncomplete,
  cronEditPatch,
  deleteCronJob,
  fetchCronJobs,
  isoToLocalInput,
  jobToEditForm,
  mutationErrorSlot,
  toggleActionFor,
  updateCronJob,
  type CronView,
} from './CronPage';

// Drives the page's real API helpers (the generated `openapi-fetch` client, so
// the route paths, the `?deleted=` query and the response decoding are all the
// production ones) against an in-memory gateway that mirrors the Rust cron
// semantics: delete is soft, pause/resume flip `status`, resume of an elapsed
// one-shot 400s, and an edit is a partial patch that reschedules from now,
// never re-arms a paused job, and is refused on a binned one.

type CronJob = components['schemas']['CronJob'];
type UpdateCronRequest = components['schemas']['UpdateCronRequest'];

const NOW = Date.parse('2026-07-14T12:00:00.000Z');
const HOUR = 60 * 60 * 1000;
const DAY = 24 * HOUR;

const EXPIRED_ONE_SHOT = 'invalid schedule: the one-shot time has already passed';
const NOT_FOUND = 'cron job not found';
const EMPTY_PATCH = 'the update sets no fields';

function job(over: Partial<CronJob> & Pick<CronJob, 'id'>): CronJob {
  return {
    user_id: 'owner',
    channel: 'tui',
    title: 'Unread digest',
    prompt: 'Summarize my unread messages',
    timezone: 'UTC',
    status: 'enabled',
    schedule: { kind: 'cron', expr: '0 9 * * *' },
    created_at: new Date(NOW - DAY).toISOString(),
    updated_at: new Date(NOW - HOUR).toISOString(),
    last_triggered_at: null,
    next_trigger_at: new Date(NOW + HOUR).toISOString(),
    deleted_at: null,
    origin_session_id: null,
    ...over,
  };
}

/** The next slot a resumed/restored job gets — never a stale or back-filled one. */
function recomputeTrigger(j: CronJob): string | null {
  if (j.schedule.kind === 'cron') return new Date(NOW + HOUR).toISOString();
  return Date.parse(j.schedule.time) > NOW ? j.schedule.time : null;
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  });
}

class FakeGateway {
  readonly seen: string[] = [];
  /** Every PATCH body the page sent, exactly as it went over the wire. */
  readonly patched: UpdateCronRequest[] = [];
  unauthorized = false;
  offline = false;

  constructor(private readonly jobs: CronJob[]) {}

  client(): AdminClient {
    return createClient<paths>({
      baseUrl: 'http://gateway.test',
      fetch: (req) => {
        if (this.offline) return Promise.reject(new Error('connection refused'));
        return this.route(req);
      },
    });
  }

  find(id: string): CronJob | undefined {
    return this.jobs.find((j) => j.id === id);
  }

  private async route(req: Request): Promise<Response> {
    const url = new URL(req.url);
    this.seen.push(`${req.method} ${url.pathname}${url.search}`);
    if (this.unauthorized) return json(401, { error: 'unauthorized' });

    const [, , , id, action] = url.pathname.split('/'); // ['', 'v1', 'cron', id?, action?]

    if (!id) {
      // GET /v1/cron — the live list by default, the recycle bin on ?deleted=true.
      const deleted = url.searchParams.get('deleted') === 'true';
      const items = this.jobs.filter((j) => Boolean(j.deleted_at) === deleted);
      return json(200, { items, next_cursor: null });
    }

    const target = this.find(id); // `get` resolves deleted rows too
    if (!target) return json(404, { error: NOT_FOUND });

    if (req.method === 'DELETE') {
      target.deleted_at = new Date(NOW).toISOString(); // soft: the row survives
      return new Response(null, { status: 204 });
    }

    if (req.method === 'PATCH') {
      return this.patch(target, (await req.json()) as UpdateCronRequest);
    }

    switch (action) {
      case 'pause':
        target.status = 'disabled';
        target.next_trigger_at = null;
        return new Response(null, { status: 204 });
      case 'resume': {
        const next = recomputeTrigger(target);
        if (next === null) return json(400, { error: EXPIRED_ONE_SHOT });
        target.status = 'enabled';
        target.next_trigger_at = next;
        return new Response(null, { status: 204 });
      }
      case 'restore': {
        target.deleted_at = null; // status is orthogonal: it comes back as it left
        if (target.status === 'enabled') {
          const next = recomputeTrigger(target);
          if (next === null) target.status = 'disabled';
          target.next_trigger_at = next;
        }
        return new Response(null, { status: 204 });
      }
      default:
        return json(404, { error: NOT_FOUND });
    }
  }

  /**
   * `PATCH /v1/cron/{id}` — the decided edit semantics: an absent field is left
   * alone; moving the schedule or the timezone recomputes the next fire time
   * from now (never back-filled); a paused job stays paused; a fired one-shot
   * re-arms on a future `at`; a binned job is refused; and a refusal writes
   * nothing at all (the draft is only committed once it validates).
   */
  private patch(target: CronJob, patch: UpdateCronRequest): Response {
    this.patched.push(patch);
    if (target.deleted_at) return json(404, { error: NOT_FOUND }); // restore it first
    if (Object.keys(patch).length === 0) return json(400, { error: EMPTY_PATCH });

    const draft: CronJob = { ...target };
    if (patch.title != null) draft.title = patch.title;
    if (patch.prompt != null) draft.prompt = patch.prompt;
    if (patch.timezone != null) draft.timezone = patch.timezone;
    if (patch.schedule != null) draft.schedule = patch.schedule;
    if (patch.schedule != null || patch.timezone != null) {
      if (draft.status === 'disabled') {
        // Load-bearing: an edit is not a resume. It keeps `disabled` and no
        // trigger — the user re-arms it explicitly.
      } else {
        const slot = recomputeTrigger(draft);
        if (slot === null) return json(400, { error: EXPIRED_ONE_SHOT }); // nothing written
        draft.status = 'enabled'; // a one-shot that already fired is re-armed
        draft.next_trigger_at = slot;
      }
    }
    draft.updated_at = new Date(NOW).toISOString();

    Object.assign(target, draft);
    return json(200, target);
  }
}

async function listOk(client: AdminClient, view: CronView): Promise<CronJob[]> {
  const outcome = await fetchCronJobs(client, view);
  if (outcome.kind !== 'ok') throw new Error(`expected an ok list, got ${outcome.kind}`);
  return outcome.items;
}

describe('toggleActionFor — which control a row offers', () => {
  it('offers pause on a running job and resume on a paused one', () => {
    expect(toggleActionFor('enabled')).toBe('pause');
    expect(toggleActionFor('disabled')).toBe('resume');
  });

  it('offers neither on a fired one-shot — there is no slot left to schedule', () => {
    expect(toggleActionFor('executed')).toBeNull();
  });
});

describe('pause / resume', () => {
  it('round-trips: pause clears the trigger, resume recomputes it from now', async () => {
    const gw = new FakeGateway([job({ id: 'c1' })]);
    const client = gw.client();

    expect(await actOnCronJob(client, 'c1', 'pause')).toEqual({ kind: 'ok' });

    const paused = (await listOk(client, 'live'))[0];
    expect(paused.status).toBe('disabled');
    expect(paused.next_trigger_at).toBeNull();
    expect(toggleActionFor(paused.status)).toBe('resume');

    expect(await actOnCronJob(client, 'c1', 'resume')).toEqual({ kind: 'ok' });

    const resumed = (await listOk(client, 'live'))[0];
    expect(resumed.status).toBe('enabled');
    expect(Date.parse(resumed.next_trigger_at ?? '')).toBeGreaterThan(NOW);
    expect(toggleActionFor(resumed.status)).toBe('pause');
  });

  it('pausing is not deleting: the job stays in the live list and out of the bin', async () => {
    const gw = new FakeGateway([job({ id: 'c1' })]);
    const client = gw.client();

    await actOnCronJob(client, 'c1', 'pause');

    expect((await listOk(client, 'live')).map((j) => j.id)).toEqual(['c1']);
    expect(await listOk(client, 'deleted')).toEqual([]);
  });

  it('resuming a one-shot whose moment has passed surfaces the backend error', async () => {
    const gw = new FakeGateway([
      job({
        id: 'c9',
        status: 'disabled',
        schedule: { kind: 'at', time: new Date(NOW - HOUR).toISOString() },
        next_trigger_at: null,
      }),
    ]);
    const client = gw.client();

    expect(await actOnCronJob(client, 'c9', 'resume')).toEqual({
      kind: 'failed',
      message: EXPIRED_ONE_SHOT,
    });

    // The failed resume left the job exactly as it was — no silent no-op that
    // paints the row "enabled" while the gateway refused it.
    const untouched = (await listOk(client, 'live'))[0];
    expect(untouched.status).toBe('disabled');
    expect(untouched.next_trigger_at).toBeNull();
  });
});

describe('recycle bin', () => {
  it('delete moves the job to the bin, restore brings it back to the live list', async () => {
    const gw = new FakeGateway([job({ id: 'c1' }), job({ id: 'c2' })]);
    const client = gw.client();

    expect(await deleteCronJob(client, 'c1')).toEqual({ kind: 'ok' });

    expect((await listOk(client, 'live')).map((j) => j.id)).toEqual(['c2']);
    const binned = await listOk(client, 'deleted');
    expect(binned.map((j) => j.id)).toEqual(['c1']);
    expect(binned[0].deleted_at).toBe(new Date(NOW).toISOString());

    expect(await actOnCronJob(client, 'c1', 'restore')).toEqual({ kind: 'ok' });

    expect((await listOk(client, 'live')).map((j) => j.id).sort()).toEqual(['c1', 'c2']);
    expect(await listOk(client, 'deleted')).toEqual([]);
    expect(gw.find('c1')?.deleted_at).toBeNull();
  });

  it('restoring an enabled job never brings back its stale trigger', async () => {
    const gw = new FakeGateway([
      job({
        id: 'c1',
        deleted_at: new Date(NOW - 3 * DAY).toISOString(),
        next_trigger_at: new Date(NOW - 3 * DAY).toISOString(), // 3 days overdue
      }),
    ]);
    const client = gw.client();

    await actOnCronJob(client, 'c1', 'restore');

    const restored = (await listOk(client, 'live'))[0];
    expect(restored.status).toBe('enabled');
    expect(Date.parse(restored.next_trigger_at ?? '')).toBeGreaterThan(NOW);
  });

  it('keeps a restored one-shot that already fired out of the schedule', async () => {
    const gw = new FakeGateway([
      job({
        id: 'c9',
        status: 'executed',
        schedule: { kind: 'at', time: new Date(NOW - DAY).toISOString() },
        deleted_at: new Date(NOW - HOUR).toISOString(),
        next_trigger_at: null,
      }),
    ]);
    const client = gw.client();

    await actOnCronJob(client, 'c9', 'restore');

    // Deletion is orthogonal to status: it comes back `executed`, not re-armed.
    const restored = (await listOk(client, 'live'))[0];
    expect(restored.status).toBe('executed');
    expect(restored.next_trigger_at).toBeNull();
    expect(toggleActionFor(restored.status)).toBeNull();
  });

  it('asks the gateway for the bin instead of filtering the live list client-side', async () => {
    const gw = new FakeGateway([job({ id: 'c1' })]);
    const client = gw.client();

    await listOk(client, 'live');
    await listOk(client, 'deleted');

    expect(gw.seen).toEqual(['GET /v1/cron?deleted=false', 'GET /v1/cron?deleted=true']);
  });
});

describe('the edit form', () => {
  it('has nothing to send until a box actually moves — including a one-shot with seconds', () => {
    const recurring = job({ id: 'c1' });
    expect(cronEditPatch(recurring, jobToEditForm(recurring))).toEqual({});

    // The `at` box holds minutes, so re-reading a job whose time carries seconds
    // must not read as a reschedule — that would re-arm a schedule nobody touched.
    const oneShot = job({
      id: 'c2',
      schedule: { kind: 'at', time: new Date(NOW + DAY + 37_000).toISOString() },
    });
    expect(cronEditPatch(oneShot, jobToEditForm(oneShot))).toEqual({});
  });

  it('blocks a save the gateway would only refuse', () => {
    const form = jobToEditForm(job({ id: 'c1' }));
    expect(cronEditIncomplete(form)).toBe(false);
    expect(cronEditIncomplete({ ...form, expr: '   ' })).toBe(true);
    expect(cronEditIncomplete({ ...form, timezone: '' })).toBe(true);
    expect(cronEditIncomplete({ ...form, prompt: '' })).toBe(true);
    expect(cronEditIncomplete({ ...form, scheduleKind: 'at', at: '' })).toBe(true);
  });
});

describe('in-place edit', () => {
  it('sends only the field the user changed, and leaves the rest of the row alone', async () => {
    const gw = new FakeGateway([job({ id: 'c1' })]);
    const client = gw.client();
    const original = (await listOk(client, 'live'))[0];

    const patch = cronEditPatch(original, {
      ...jobToEditForm(original),
      prompt: 'Summarize only the unread DMs',
    });
    expect(patch).toEqual({ prompt: 'Summarize only the unread DMs' });

    const outcome = await updateCronJob(client, 'c1', patch);
    if (outcome.kind !== 'ok') throw new Error(`expected an edited job, got ${outcome.kind}`);

    // The body on the wire carries the changed field and nothing else: a PATCH
    // that re-sent the schedule would recompute the next fire from now.
    expect(gw.patched).toEqual([{ prompt: 'Summarize only the unread DMs' }]);
    expect(gw.seen).toContain('PATCH /v1/cron/c1');

    const edited = (await listOk(client, 'live'))[0];
    expect(edited.id).toBe(original.id); // same id: the job keeps its history
    expect(edited.prompt).toBe('Summarize only the unread DMs');
    expect(edited.title).toBe(original.title);
    expect(edited.timezone).toBe(original.timezone);
    expect(edited.schedule).toEqual(original.schedule);
    expect(edited.next_trigger_at).toBe(original.next_trigger_at); // not re-armed
  });

  it('reschedules from now: the row shows the new trigger, never a back-filled one', async () => {
    const gw = new FakeGateway([
      job({ id: 'c1', next_trigger_at: new Date(NOW + 6 * HOUR).toISOString() }),
    ]);
    const client = gw.client();
    const original = (await listOk(client, 'live'))[0];

    const patch = cronEditPatch(original, { ...jobToEditForm(original), expr: '30 7 * * *' });
    expect(patch).toEqual({ schedule: { kind: 'cron', expr: '30 7 * * *' } });

    const outcome = await updateCronJob(client, 'c1', patch);
    if (outcome.kind !== 'ok') throw new Error(`expected an edited job, got ${outcome.kind}`);

    const rescheduled = (await listOk(client, 'live'))[0];
    expect(rescheduled.schedule).toEqual({ kind: 'cron', expr: '30 7 * * *' });
    expect(rescheduled.status).toBe('enabled');
    expect(rescheduled.next_trigger_at).not.toBe(original.next_trigger_at);
    expect(Date.parse(rescheduled.next_trigger_at ?? '')).toBeGreaterThan(NOW);

    // The edit answers with the trigger the row then shows, so the page has the
    // new time in hand without guessing it client-side.
    expect(outcome.job.next_trigger_at).toBe(rescheduled.next_trigger_at);
  });

  it('leaves a paused job paused — an edit is not a resume', async () => {
    const gw = new FakeGateway([job({ id: 'c1', status: 'disabled', next_trigger_at: null })]);
    const client = gw.client();
    const original = (await listOk(client, 'live'))[0];

    const patch = cronEditPatch(original, {
      ...jobToEditForm(original),
      title: 'Evening digest',
      expr: '0 21 * * *',
    });
    expect(patch).toEqual({
      title: 'Evening digest',
      schedule: { kind: 'cron', expr: '0 21 * * *' },
    });

    const outcome = await updateCronJob(client, 'c1', patch);
    if (outcome.kind !== 'ok') throw new Error(`expected an edited job, got ${outcome.kind}`);
    expect(outcome.job.status).toBe('disabled');
    expect(outcome.job.next_trigger_at).toBeNull();

    // The edit landed, but the row still reads paused and offers Resume — a save
    // that quietly restarted the job would be the bug this pins.
    const paused = (await listOk(client, 'live'))[0];
    expect(paused.title).toBe('Evening digest');
    expect(paused.schedule).toEqual({ kind: 'cron', expr: '0 21 * * *' });
    expect(paused.status).toBe('disabled');
    expect(paused.next_trigger_at).toBeNull();
    expect(toggleActionFor(paused.status)).toBe('resume');
  });

  it('re-arms a one-shot that already fired when it is given a time in the future', async () => {
    const fired = new Date(NOW - DAY).toISOString();
    const gw = new FakeGateway([
      job({
        id: 'c9',
        status: 'executed',
        schedule: { kind: 'at', time: fired },
        last_triggered_at: fired,
        next_trigger_at: null,
      }),
    ]);
    const client = gw.client();
    const original = (await listOk(client, 'live'))[0];

    const tomorrow = new Date(NOW + DAY).toISOString();
    const patch = cronEditPatch(original, {
      ...jobToEditForm(original),
      at: isoToLocalInput(tomorrow),
    });
    expect(patch).toEqual({ schedule: { kind: 'at', time: tomorrow } });

    const outcome = await updateCronJob(client, 'c9', patch);
    if (outcome.kind !== 'ok') throw new Error(`expected an edited job, got ${outcome.kind}`);

    // This is why editing beats delete + recreate: the job is armed again while
    // keeping its id and the runs it already has.
    expect(outcome.job.id).toBe('c9');
    expect(outcome.job.status).toBe('enabled');
    expect(outcome.job.next_trigger_at).toBe(tomorrow);
    expect(outcome.job.last_triggered_at).toBe(fired);
    expect(toggleActionFor(outcome.job.status)).toBe('pause');
  });

  it('refuses an `at` that has already passed, and the refusal belongs in the modal', async () => {
    const gw = new FakeGateway([job({ id: 'c1' })]);
    const client = gw.client();
    const original = (await listOk(client, 'live'))[0];

    const past = new Date(NOW - DAY).toISOString();
    const patch = cronEditPatch(original, {
      ...jobToEditForm(original),
      scheduleKind: 'at',
      at: isoToLocalInput(past),
    });
    expect(patch).toEqual({ schedule: { kind: 'at', time: past } });

    const outcome = await updateCronJob(client, 'c1', patch);
    if (outcome.kind !== 'failed') throw new Error(`expected a refusal, got ${outcome.kind}`);
    expect(outcome.message).toBe(EXPIRED_ONE_SHOT);

    // The edit modal is the topmost dialog when this lands, so the message has to
    // paint inside it; on the page it would sit under the modal's own overlay.
    expect(mutationErrorSlot(outcome.message, 'edit')).toBe('edit');
    expect(mutationErrorSlot(outcome.message, null)).toBe('page');

    // And the refused edit wrote nothing: no half-applied schedule.
    const untouched = (await listOk(client, 'live'))[0];
    expect(untouched.schedule).toEqual(original.schedule);
    expect(untouched.status).toBe('enabled');
    expect(untouched.next_trigger_at).toBe(original.next_trigger_at);
  });

  it('refuses to edit a job in the recycle bin — it has to be restored first', async () => {
    const gw = new FakeGateway([
      job({ id: 'c1', deleted_at: new Date(NOW - HOUR).toISOString() }),
    ]);
    const client = gw.client();
    const binned = (await listOk(client, 'deleted'))[0];

    const outcome = await updateCronJob(
      client,
      'c1',
      cronEditPatch(binned, { ...jobToEditForm(binned), prompt: 'Never lands' }),
    );
    expect(outcome).toEqual({ kind: 'failed', message: NOT_FOUND });
    expect(mutationErrorSlot(NOT_FOUND, 'edit')).toBe('edit');

    const stillBinned = (await listOk(client, 'deleted'))[0];
    expect(stillBinned.prompt).toBe(binned.prompt);
  });

  it('refuses a patch that sets nothing — which is why an untouched form cannot be saved', async () => {
    const gw = new FakeGateway([job({ id: 'c1' })]);
    const client = gw.client();

    expect(await updateCronJob(client, 'c1', {})).toEqual({
      kind: 'failed',
      message: EMPTY_PATCH,
    });
  });
});

describe('failure mapping', () => {
  it('reports a 404 for a job that is no longer there', async () => {
    const client = new FakeGateway([]).client();
    expect(await actOnCronJob(client, 'gone', 'pause')).toEqual({
      kind: 'failed',
      message: NOT_FOUND,
    });
  });

  it('asks the caller to log out on a 401 rather than showing a bogus error', async () => {
    const gw = new FakeGateway([job({ id: 'c1' })]);
    gw.unauthorized = true;
    const client = gw.client();

    expect(await actOnCronJob(client, 'c1', 'restore')).toEqual({ kind: 'unauthorized' });
    expect(await fetchCronJobs(client, 'deleted')).toEqual({ kind: 'unauthorized' });
    expect(await updateCronJob(client, 'c1', { title: 'Nope' })).toEqual({ kind: 'unauthorized' });
  });

  it('turns a dead connection into a message instead of an unhandled rejection', async () => {
    const gw = new FakeGateway([job({ id: 'c1' })]);
    gw.offline = true;
    const client = gw.client();

    expect(await actOnCronJob(client, 'c1', 'pause')).toEqual({
      kind: 'failed',
      message: 'Network error: connection refused',
    });
    expect(await fetchCronJobs(client, 'live')).toEqual({
      kind: 'failed',
      message: 'Network error: connection refused',
    });
    expect(await updateCronJob(client, 'c1', { title: 'Nope' })).toEqual({
      kind: 'failed',
      message: 'Network error: connection refused',
    });
  });

  it('paints a failure on the page only when no modal is covering it', () => {
    expect(mutationErrorSlot(null, null)).toBe('none');
    expect(mutationErrorSlot(null, 'edit')).toBe('none');
    expect(mutationErrorSlot(NOT_FOUND, null)).toBe('page');
    expect(mutationErrorSlot(NOT_FOUND, 'detail')).toBe('detail');
    expect(mutationErrorSlot(NOT_FOUND, 'trash')).toBe('trash');
  });
});
