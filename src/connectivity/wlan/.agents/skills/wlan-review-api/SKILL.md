---
name: wlan-review-api
description: >
  Reviews Fuchsia WLAN FIDL API changes against platform and WLAN guidelines.
  Use when reviewing CLs touching WLAN FIDL files or FIDL.md. Do NOT use as a
  standalone review orchestrator (use /review instead) or for non-FIDL
  changes.
---

## When to use this skill

- Use this when reviewing a Fuchsia CL that includes any WLAN FIDL or API
  changes.

## How to use it

Ensure the FIDL API follows the guidance in:

- docs/development/api/fidl.md: FIDL API readability rubric.
- docs/development/languages/fidl/guides/style.md: FIDL style guide.
- src/connectivity/wlan/FIDL.md: WLAN-specific FIDL API guidance.
