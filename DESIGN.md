# Ultradian Design

## Overview

The interface should resemble a well-run terminal session: high signal, restrained color, stable alignment, and enough whitespace to reveal hierarchy without making output long. The physical scene is an engineer moving between a bright office terminal, a remote shell, and an agent transcript; the presentation must survive all three without assuming a dark theme or a particular font.

Runtime output maps semantic roles to portable ANSI foreground colors and never paints the terminal background. Plain mode removes the mapping without changing wording, symbols, indentation, or hierarchy.

## Color roles

- **Brand (magenta):** the brand mark, selected item, and the one strongest heading on a surface.
- **Accent (cyan):** commands, flags, paths, and links.
- **Success, warning, and error:** reserved for actual state. Each is paired with explicit copy and a stable symbol.
- **Muted (dim):** secondary descriptions, hints, defaults, and table metadata. Important instructions never rely on dim text alone.

## Typography

The terminal owns the typeface. Hierarchy comes from weight, indentation, whitespace, and sparing color. There is no simulated display typography.

- Product and section headings use bold.
- Commands, flags, paths, IDs, and values retain monospace terminal text.
- Uppercase is limited to compact status labels. Prose headings use sentence case.
- Wrapped prose targets roughly 72 columns where practical.

## Layout

Spacing uses terminal lines and columns:

- One blank line separates major sections.
- Related label/value rows stay contiguous.
- Descriptions align to the widest command or flag within a group, within a bounded gutter.
- Examples are shell-ready and appear before exhaustive option prose in command help.
- Root help is short enough to scan without paging; deeper information belongs in command help and `describe`.

The output order is stable: identity and outcome, primary data, warnings or follow-up, then optional hints. JSON and JSONL modes omit all visual layout.

Terminal hierarchy is flat. Weight, indentation, semantic color, and blank lines replace cards, borders, and shadows.

## Symbols

Symbols are small state markers:

- `✓` / `[ok]` success
- `!` / `[warn]` warning
- `✗` / `[error]` failure
- `●` / `[*]` active work
- `○` / `[-]` pending or inactive

ASCII fallbacks preserve the same column width where possible.

## Components

**Brand line:** a single brand-colored mark, CLI name, and muted version. It appears on root help and interactive onboarding only.

**Help:** usage, one-sentence purpose, grouped commands, global options, examples, and a final discovery hint. Command help leads with runnable examples before exhaustive options.

**Result:** human renderers receive typed data and return strings; they do not print directly. The engine owns stdout and stderr.

**Error:** starts with a stable error label and human message, followed by a `hint:` or `docs:` line when actionable. JSON mode emits the same code, message, and remediation fields.

**Prompt:** line-oriented prompts only. The user sees the decision and default before input. Cancellation returns a named result, never a stack trace.

**Progress:** a spinner may be used for indeterminate interactive work longer than a perceptible instant. Non-TTY and machine modes receive quiet execution or explicit stderr lifecycle lines.

**Table:** borderless, left-aligned, bold header row, no color in cell values unless it communicates state. Column padding is capped so one long value cannot push every other column off screen.

## Do's and Don'ts

- Do keep stdout usable as data and stderr usable as diagnostics.
- Do pair every color with text, position, or a symbol.
- Do show a copy-pasteable recovery command when the user can fix the problem.
- Do make `NO_COLOR=1` output a first-class test target.
- Don't print the brand line before every command.
- Don't animate, prompt, or emit ANSI sequences in JSON, JSONL, CI, or non-TTY modes.
- Don't use emoji as the only status indicator.
- Don't truncate IDs or paths in machine output.
- Don't let module code call `console.log` directly.
