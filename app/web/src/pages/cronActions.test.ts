import { describe, expect, it } from 'vitest';
import createClient from 'openapi-fetch';

import type { AdminClient } from '../api/client';
import type { components, paths } from '../api/schema';
import {
  actOnCronJob,
  deleteCronJob,
  fetchCronJobs,
  toggleActionFor,
  type CronView,
} from './CronPage';

// Drives the page's real API helpers (the generated `openapi-fetch` client, so
// the route paths, the `?deleted=` query and the response decoding are all the
// production ones) against an in-memory gateway that mirrors the Rust cron
// semantics: delete is soft, pause/resume flip `status`, and resume of an
// elapsed one-shot 400s.

type CronJob = components['schemas']['CronJob'];

const NOW = Date.parse('2026-07-14T12:00:00.000Z');
const HOUR = 60 * 60 * 1000;
const DAY = 24 * HOUR;

const EXPIRED_ONE_SHOT = 'invalid schedule: the one-shot time has already passed';
const NOT_FOUND = 'cron job not found';

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
  unauthorized = false;
  offline = false;

  constructor(private readonly jobs: CronJob[]) {}

  client(): AdminClient {
    return createClient<paths>({
      baseUrl: 'http://gateway.test',
      fetch: (req) => {
        if (this.offline) return Promise.reject(new Error('connection refused'));
        return Promise.resolve(this.route(req));
      },
    });
  }

  find(id: string): CronJob | undefined {
    return this.jobs.find((j) => j.id === id);
  }

  private route(req: Request): Response {
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
  });
});
