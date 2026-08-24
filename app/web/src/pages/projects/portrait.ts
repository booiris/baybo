import { useEffect, useMemo, useState } from 'react';

import { useAuth } from '../../api/auth';
import { blobObjectUrl } from '../../api/blobs';
import { botttsFace } from '../../components/botttsFace';
import type { Agent } from './boardModel';

/// The picture to draw for somebody on this board, or null for the operator
/// and the board itself — neither is an agent, and neither has a face.
export type Portrait = (agentId: string | null | undefined) => string | null;

/// What a surface draws when it has no roster to consult: the generated
/// face. An agent id is the whole input, which is why a component that never
/// receives the team still draws the right teammate.
export const generatedPortrait: Portrait = (agentId) =>
  agentId == null || agentId === '' ? null : botttsFace(agentId);

const NO_FACES = new Map<string, string>();

/// Every portrait this board needs, uploaded ones included.
///
/// One hook for the whole team rather than a `useBlobUrl` per avatar: the
/// same teammate is drawn on each card it owns, on each comment it wrote and
/// again in the header, and a hook per drawing fetches the same bytes once
/// per drawing.
export function useTeamPortraits(team: Agent[]): Portrait {
  const { baseUrl, token } = useAuth();
  // A string rather than the array, so a poll that returns an equal roster
  // does not re-run the effect. It pairs each agent with its blob, because
  // an agent whose avatar was replaced needs the new one fetched.
  const wanted = team
    .filter((agent) => agent.avatar_blob_id != null && agent.avatar_blob_id !== '')
    .map((agent) => `${agent.id} ${agent.avatar_blob_id ?? ''}`)
    .join('\n');
  const [uploaded, setUploaded] = useState(NO_FACES);

  useEffect(() => {
    if (wanted === '') {
      setUploaded(NO_FACES);
      return;
    }
    let alive = true;
    const pairs = wanted.split('\n').map((line) => {
      const cut = line.indexOf(' ');
      return { id: line.slice(0, cut), blobId: line.slice(cut + 1) };
    });
    void Promise.all(
      pairs.map(async ({ id, blobId }) => {
        try {
          return [id, await blobObjectUrl(baseUrl, blobId, token)] as const;
        } catch {
          // One unreachable blob costs that agent its upload, not the board
          // its faces — it falls back to a generated one like everybody else.
          return null;
        }
      }),
    ).then((faces) => {
      if (alive) setUploaded(new Map(faces.filter((face) => face !== null)));
    });
    return () => {
      alive = false;
    };
  }, [wanted, baseUrl, token]);

  return useMemo<Portrait>(
    () => (agentId) => {
      if (agentId == null || agentId === '') return null;
      return uploaded.get(agentId) ?? botttsFace(agentId);
    },
    [uploaded],
  );
}
