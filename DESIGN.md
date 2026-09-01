---
name: Holt
description: A calm, precise native workspace for coding-agent sessions.
colors:
  background-dark: "#060606"
  shell-dark: "#0d0d0d"
  raised-dark: "#343438"
  background-light: "#ffffff"
  shell-light: "#f3f3f5"
  raised-light: "#ededf0"
  text-dark: "#e8e8ea"
  text-light: "#303035"
  text-muted-dark: "#a9a9ae"
  text-muted-light: "#62626a"
  accent-dark: "#8b7cf6"
  accent-light: "#5b43e8"
  danger: "#dc2626"
  warning: "#a16207"
  success: "#15803d"
  ink: "#000000"
typography:
  display:
    fontFamily: "Geist, -apple-system, BlinkMacSystemFont, sans-serif"
    fontSize: "20px"
    fontWeight: 600
    lineHeight: 1.2
  title:
    fontFamily: "Geist, -apple-system, BlinkMacSystemFont, sans-serif"
    fontSize: "16px"
    fontWeight: 600
    lineHeight: 1.35
  body:
    fontFamily: "Geist, -apple-system, BlinkMacSystemFont, sans-serif"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1.5
  label:
    fontFamily: "Geist, -apple-system, BlinkMacSystemFont, sans-serif"
    fontSize: "12px"
    fontWeight: 500
    lineHeight: 1.3
    letterSpacing: "0.01em"
  code:
    fontFamily: "Geist Mono, Menlo, monospace"
    fontSize: "12px"
    fontWeight: 400
    lineHeight: 1.5
rounded:
  control: "6px"
  panel: "10px"
  bubble: "16px"
  pill: "8px"
spacing:
  xs: "4px"
  sm: "8px"
  md: "12px"
  lg: "16px"
components:
  button-primary:
    backgroundColor: "{colors.accent-light}"
    textColor: "{colors.background-light}"
    rounded: "{rounded.control}"
    padding: "8px 12px"
  button-ghost:
    backgroundColor: "transparent"
    textColor: "{colors.text-light}"
    rounded: "{rounded.control}"
    padding: "6px 8px"
  input:
    backgroundColor: "{colors.background-light}"
    textColor: "{colors.text-light}"
    rounded: "{rounded.control}"
    padding: "8px 10px"
  chip-selected:
    backgroundColor: "{colors.accent-light}"
    textColor: "{colors.background-light}"
    rounded: "{rounded.pill}"
    padding: "4px 8px"
  panel:
    backgroundColor: "{colors.background-light}"
    textColor: "{colors.text-light}"
    rounded: "{rounded.panel}"
    padding: "16px"
---

# Design System: Holt

## Overview

**Creative North Star: "The Quiet Workbench"**

Holt is a native engineering surface: compact, dependable, and legible at a glance. The interface keeps the coding task primary while chrome recedes into carefully ordered planes. Dark and light appearances are authored as distinct scenes, not inverted screenshots. Frosted shell surfaces can carry atmosphere, while content remains anchored by explicit borders, tonal layers, and predictable desktop affordances.

The visual language is focused, precise, and calm. It rejects dashboard decoration, nested cards, hidden state, ambiguous icons, and web-shaped UI. Motion is feedback for a state change, never a performance. Density is intentional, with generous breathing room at panel boundaries and tighter rhythm inside controls.

**Key Characteristics:**
- Near-achromatic surfaces with a single Holt indigo accent.
- Geist for interface text, Geist Mono for code and terminal output.
- Small radii and restrained elevation; selection is carried by tonal wash and an inset ring.
- Keyboard-first, explicit states across menus, pickers, transcript, terminal, and composer.

## Colors

The palette is restrained: tinted neutrals do the structural work, while one authored accent marks action, focus, and activity. Semantic red, amber, and green are reserved for status meaning.

### Primary
- **Holt Indigo (dark)** (#8b7cf6): Interactive accent, caret, code links, active states, and the animated glyph on near-black surfaces.
- **Holt Indigo (light)** (#5b43e8): Contrast-corrected sibling for light surfaces; use for the same semantic roles.

### Neutral
- **Obsidian** (#060606): Dark content background.
- **Graphite Shell** (#0d0d0d): Dark sidebar/titlebar plane.
- **Raised Graphite** (#343438): Dark raised controls and frosted highlights.
- **Paper** (#ffffff): Light content and popover plane.
- **Mist Shell** (#f3f3f5): Light chrome and sidebar plane.
- **Raised Mist** (#ededf0): Light raised controls.
- **Ink** (#303035): Primary light-mode text.
- **Soft White** (#e8e8ea): Primary dark-mode text.
- **Muted Ink** (#62626a / #a9a9ae): Secondary labels and supporting copy, appearance-specific.

### Named Rules
**The One Accent Rule.** Indigo is the only general-purpose accent. Do not introduce decorative gradients or unrelated saturated colors; semantic colors appear only when communicating status.

## Typography

**Display Font:** Geist (with system UI fallback)
**Body Font:** Geist (with system UI fallback)
**Label/Mono Font:** Geist Mono (with Menlo fallback)

**Character:** Geist is compact and neutral, giving dense engineering surfaces a human reading rhythm. Geist Mono is reserved for code, terminal, file paths, and keyboard-like tokens so technical content is visibly distinct without becoming theatrical.

### Hierarchy
- **Display** (600, 20px, 1.2): Settings page titles and high-level empty states.
- **Title** (600, 16px, 1.35): Panel headings, session names, and composer labels.
- **Body** (400, 13px, 1.5): Transcript text, descriptions, and settings copy. Keep prose to roughly 65–75ch where width allows.
- **Label** (500, 12px, 1.3, slight tracking): Buttons, tabs, metadata, and compact controls.
- **Code** (400, 12px, 1.5): Terminal, diffs, paths, and inline code using Geist Mono.

### Named Rules
**The Two-Texture Rule.** Never use the mono face for ordinary prose, and never render code or terminal output in proportional Geist.

## Elevation

Holt uses tonal layering first and shadows second. Dark surfaces lift through a restrained light wash; light surfaces lift through white, a hairline border, and a soft shadow. Frost is structural and purposeful in the shell, never a decorative glass card applied to every surface.

### Shadow Vocabulary
- **Menu lift** (`shadow-md`): Anchored pickers, tooltips, and compact menus above the workbench.
- **Panel lift** (`shadow-lg`): Dialogs and larger floating command surfaces.
- **Selection ring** (inset 1px, dark white at 9% / light black at 7%): Selected rows and chips without adding layout width.

### Named Rules
**The Flat-by-Default Rule.** Resting content is flat. Elevation appears only for a floating layer or an interaction state, and never as a permanent stack of nested cards.

## Components

### Buttons
- **Shape:** Small, familiar control radius (6px), with 6–12px vertical/horizontal padding.
- **Primary:** Holt Indigo fill, high-contrast label, and a compact hit target for submit or confirm actions.
- **Hover / Focus:** Use the theme hover wash and a visible focus ring; transitions are short and ease-out. Never animate layout.
- **Ghost:** Transparent at rest, tonal hover fill, muted text until the action is available.

### Chips
- **Style:** 8px pill radius, 4px vertical and 8px horizontal padding, muted ink wash at rest.
- **State:** Selected chips use the accent wash plus an inset ring; keyboard-active chips use the same treatment as pointer selection.

### Cards / Containers
- **Corner Style:** Panels use 10px; message bubbles use 16px; controls stay at 6px.
- **Background:** Assign one theme surface per plane: content, shell, raised, or card. Do not stack opaque cards to manufacture hierarchy.
- **Shadow Strategy:** Follow the Elevation ladder. Floating cards may use a soft shadow; in-card selections use the inset ring only.
- **Border:** Hairline theme border, especially in light mode where tonal contrast is intentionally close.
- **Internal Padding:** 8px for compact rows, 12px for grouped controls, 16px for panel content.

### Inputs / Fields
- **Style:** Theme input surface, 6px radius, 8–10px horizontal padding, and a quiet hairline border.
- **Focus:** Accent caret and visible focus ring; focus must remain obvious in both appearances.
- **Error / Disabled:** Semantic danger text and muted fill for errors/disabled states; preserve readable labels and do not rely on color alone.

### Navigation
- **Style:** The 38px titlebar and tab rail use compact 12px labels, 2–8px internal gaps, and explicit active washes. Menus and pickers are keyboard navigable, anchored, and dismissed predictably.

### Composer
The composer is the primary action surface: a broad 26px-radius pill with a restrained glass or panel fill, clear provider/model state, and a 24px reserved status strip so execution feedback never shifts the layout.

## Do's and Don'ts

### Do:
- **Do** keep the coding task primary and chrome quiet.
- **Do** use authored dark/light accent pairs so interactive text meets contrast in each appearance.
- **Do** make provider, model, and execution state explicit before an action runs.
- **Do** preserve keyboard focus, reduced-motion behavior, and labels for icon-only controls.
- **Do** use 4/8/12/16px spacing steps and 6/10/16px radius steps consistently.

### Don't:
- **Don't** add dashboard decoration, nested cards, or animation that competes with the task.
- **Don't** hide state, use ambiguous icons, or introduce web-shaped UI that feels foreign in a native tool.
- **Don't** use colored side-stripe borders, gradient text, or glassmorphism as a default surface treatment.
- **Don't** create identical card grids with icon-plus-heading-plus-copy repeated across a screen.
- **Don't** use a modal as the first thought when inline or progressive disclosure can work.
- **Don't** use `#000` or `#fff` for text or decoration outside the authored theme surfaces.
