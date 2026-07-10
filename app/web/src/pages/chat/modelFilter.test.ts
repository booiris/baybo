import { describe, expect, it } from 'vitest';
import { filterAllowedModels } from './modelFilter';

describe('filterAllowedModels', () => {
  const models = [{ name: 'a' }, { name: 'b' }, { name: 'c' }];
  it('empty set = unrestricted', () => {
    expect(filterAllowedModels(models, [])).toHaveLength(3);
  });
  it('filters to members, preserving model order', () => {
    expect(filterAllowedModels(models, ['c', 'a']).map((m) => m.name)).toEqual(['a', 'c']);
  });
  it('unknown members filter everything they do not match', () => {
    expect(filterAllowedModels(models, ['zzz'])).toHaveLength(0);
  });
});
