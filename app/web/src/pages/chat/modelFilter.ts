/** Filters a model list down to an agent's `allowed_models` set. An empty
 *  set means "unrestricted" — every configured model stays visible. */
export function filterAllowedModels<T extends { name: string }>(
  models: T[],
  allowed: string[],
): T[] {
  if (allowed.length === 0) return models;
  return models.filter((m) => allowed.includes(m.name));
}
