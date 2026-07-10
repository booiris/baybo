import { render, screen, fireEvent } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { AgentPicker, type AgentOption } from './AgentPicker';

const agents: AgentOption[] = [
  { id: 'baybo', name: 'baybo', description: 'default', builtin: true, framework: 'baybo' },
  { id: 'A1', name: 'Helper', description: 'helps', builtin: false, framework: 'baybo' },
  { id: 'A2', name: 'Coder', description: 'claude-backed', builtin: false, framework: 'claude' },
];

describe('AgentPicker', () => {
  it('lists agents builtin-first and picks by click', () => {
    const onPick = vi.fn();
    render(<AgentPicker agents={agents} onPick={onPick} onClose={() => {}} />);
    const rows = screen.getAllByRole('button');
    expect(rows[0]).toHaveTextContent('baybo');
    fireEvent.click(screen.getByText('Helper'));
    expect(onPick).toHaveBeenCalledWith('A1');
  });

  it('maps the builtin to null and disables external-framework agents', () => {
    const onPick = vi.fn();
    render(<AgentPicker agents={agents} onPick={onPick} onClose={() => {}} />);
    fireEvent.click(screen.getByText('baybo'));
    expect(onPick).toHaveBeenCalledWith(null);
    expect(screen.getByText('Coder').closest('button')).toBeDisabled();
  });
});
