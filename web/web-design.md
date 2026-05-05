# Aura Web Design System

This document outlines the design principles, visual identity, and component patterns used in the Aura Dashboard.

## Design Aesthetic: Neo-Brutalism

The Aura Dashboard follows a "Neo-Brutalist" design language characterized by:

- **High Contrast:** Sharp contrasts between white, gray, and black.
- **Heavy Borders:** Consistent use of 2px or 3px solid black borders (`border-black`).
- **Hard Shadows:** Off-set black shadows without blur (`shadow-brutal`).
- **Geometric Rigidity:** Minimal use of soft rounded corners (standard radius is 6px).
- **Technical Typography:** Heavy use of monospaced fonts to emphasize the "system" nature of the tool.

## Visual Identity

### Color Palette

| Name            | Hex       | Usage                             |
| :-------------- | :-------- | :-------------------------------- |
| **Canvas**      | `#fafafa` | Primary background color          |
| **Ink**         | `#111111` | Primary text and borders          |
| **Ink Soft**    | `#555555` | Secondary text and metadata       |
| **Brand**       | `#3b60e4` | Primary actions and active states |
| **Brand Hover** | `#3151c4` | Action hover states               |
| **Error**       | `#e53e3e` | Destructive actions, Error logs   |
| **Warning**     | `#dd6b20` | Warning logs                      |
| **Info**        | `#3182ce` | Informational logs                |
| **OK**          | `#2f855a` | Success states                    |

### Typography

- **Primary Font:** `Space Mono` (Monospace) - Used for body text, UI labels, and data.
- **Secondary Font:** `Inter` (Sans-serif) - Used as a fallback or for specific readability needs.
- **Styles:**
  - Headers: Bold, uppercase, tight tracking.
  - UI Labels: Bold, uppercase, tracking-wider.

### Shadows & Borders

- **Shadow Brutal:** `4px 4px 0 0 #000`
- **Shadow Brutal SM:** `3px 3px 0 0 #000`
- **Radius:** `6px` (`radius-brutal`)
- **Borders:** `2px` or `3px` solid `#000`.

## Layout Structure

### 1. Authentication Layer

- **Login Screen:** Centered "brutal" box containing brand identity and token input.

### 2. Main Dashboard

- **Sidebar:** Fixed 180px width, containing brand logo ("AURA"), version, and navigation links.
- **Top Navigation:** Contains breadcrumbs and contextual information.
- **Content Area:** Scrollable main view with `bg-canvas`.

## Components

### Buttons

- **Standard Button:** Large, bold uppercase text, black border, brand background for primary.
- **Icon Button:** Square or circular, 2px border, used for compact actions.
- **Interaction:** Buttons often "push down" (translate 2px/2px) on active state, removing the shadow to simulate a physical press.

### Inputs

- **SearchBox:** Minimalist, with search icon and 2px border.
- **SelectBox:** Custom styled to match the brutalist aesthetic.

## Iconography

- **Library:** [Remix Icon](https://remixicon.com/) (via `react-icons/ri`).
- **Style:** Outlined or filled depending on emphasis, usually `text-xl`.
