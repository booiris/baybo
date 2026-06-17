# Aura macOS app (`app/mac`)

Tauri v2 shell that embeds the Aura runtime in-process, with a fresh React 19 +
Vite + Tailwind v4 frontend. Full architecture + milestones: `docs/mac-app.md`.

- **Stack**: React 19 + TypeScript + Vite + Tailwind v4. Warm neo-brutalist
  design tokens live in `src/index.css` under the `@theme` block (cream canvas,
  gold rail, coral selected, bold black borders + hard offset shadows, monospace
  metadata). Do **not** reuse `web/`'s tokens.
- **Backend connection**: `src/api/{client,chatWs}.ts` are ported from `web/`
  (connection code only); regenerate types with `pnpm --filter aura-mac gen:api`.
- **Run / build**: `./build.sh` (`run` | `build` | `web` | `clean`).

## UI conventions

- **No native `<select>` dropdowns.** The browser's native popup can't be themed
  and clashes with the design. Use the custom `src/components/Select.tsx`
  (warm-brutalist styled; `size="md"` matches text-input height, `size="sm"` for
  tight spots like the chat header) for **every** dropdown.
- Hide a scroll region's scrollbar with the `.no-scrollbar` utility (defined in
  `src/index.css`) — scrolling still works, the bar is just not shown.
