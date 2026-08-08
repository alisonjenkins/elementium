# Patches carried against Element Web

**Generated** by `just element-web-patches` — do not edit by hand.

Every row is a commit on `elementium` in [the fork](https://github.com/alisonjenkins/element-web), rebased onto
`v1.12.25`. A change upstream accepts disappears from this list at the next rebase,
because rebase drops a commit whose patch-id upstream already has — so an empty list
means we carry nothing, not that nobody has updated the file.

**Nothing is currently carried.** Element Web is used unmodified at `v1.12.25`;
everything Elementium changes is a runtime shim or build-time injection, which
`docs/element-web.md` explains the reasoning for.
