/** The `reasoning_effort` vocabulary the agent editor offers, in ascending
 *  order — mirrors `baybo_model::ReasoningEffort::ALL`. Single source so
 *  the select options can't drift from the values the server accepts. */
export const REASONING_EFFORT_LEVELS = ['minimal', 'low', 'medium', 'high', 'xhigh'] as const;

/** Filters a model list down to an agent's `allowed_models` set. An empty
 *  set means "unrestricted" — every configured model stays visible. */
export function filterAllowedModels<T extends { name: string }>(
  models: T[],
  allowed: string[],
): T[] {
  if (allowed.length === 0) return models;
  return models.filter((m) => allowed.includes(m.name));
}
