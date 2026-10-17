import { createContext, useContext } from 'react';

/** Opens the chat page's browser panel. Absent outside the chat page (the
 *  board's run transcript reuses the thread renderer), where tool steps then
 *  offer no `view` affordance. */
export const OpenBrowserPanelContext = createContext<(() => void) | null>(null);

export function useOpenBrowserPanel(): (() => void) | null {
  return useContext(OpenBrowserPanelContext);
}
