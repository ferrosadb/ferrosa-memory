---
title: Trust and Permissions — Canonical Spec Pointer
executive_summary:
  purpose: >-
    Points to the shared trust, identity and permissions specification. This
    repository does not hold a copy, because a security model duplicated across
    three repositories diverges silently.
  critical_items:
    - Canonical location is ferrosa-suite/specs/trust/.
    - Do not copy the spec here; update it at the source.
status: accepted
last_reviewed: 2026-08-21
---

# Trust and Permissions — Canonical Spec

The identity, admission, authorization and sharing model shared by
`ferrosa-memory`, `ferrosa-mobile` and `ferrosa-workbench` lives at:

    ferrosa-suite/specs/trust/decisions.md

**This repository deliberately holds a pointer, not a copy.** A security model
duplicated into three repositories diverges, and the divergence stays invisible
until two components disagree about who may do what — precisely the class of bug
the spec exists to prevent.

To change the model, change it at the canonical location. To read it, read it
there.
