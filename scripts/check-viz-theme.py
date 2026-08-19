#!/usr/bin/env python3
"""Guard the theme token blocks in the memory web assets.

Each asset declares its light palette twice: once for an explicit
`data-theme="light"` choice, and once inside `@media (prefers-color-scheme:
light)` for hosts that asked for light without the user picking. CSS cannot
alias one block from the other, so they are duplicated — and duplicated blocks
drift. They already did once: `--film` was added to the explicit block and the
media copy silently kept the old palette, which is invisible until someone
views the page with a light system preference and no saved choice.

This fails loudly on three things:
  1. the two light blocks declaring different token sets or values,
  2. a token declared in dark with no light counterpart (it would keep its
     dark-ground value on a light ground),
  3. colour literals painted in rules instead of tokens, which cannot flip.
"""
import re
import sys
from pathlib import Path

ASSETS = ["crates/ferrosa-memory-core/assets/viz.html",
          "crates/ferrosa-memory-core/assets/workbench.html"]

# Tokens that are legitimately dark-only: they carry no colour.
NON_COLOUR = {"--font-sans", "--font-serif", "--font-mono", "--panel-top",
              "--radius-xs", "--radius-sm", "--radius-md", "--radius-lg",
              "--radius-pill"}


def tokens(block: str) -> dict:
    return dict(re.findall(r"^\s*(--[a-z0-9-]+)\s*:\s*([^;]+);", block, re.M))


def check(path: Path) -> list:
    src = path.read_text()
    if "<style>" not in src:
        return [f"{path}: no <style> block"]
    fails = []

    # Assets differ in indentation, so match structurally rather than by column.
    dark = re.search(r"^[ \t]*:root \{\n(.*?)^[ \t]*\}", src, re.S | re.M)
    light = re.search(r'^[ \t]*:root\[data-theme="light"\] \{\n(.*?)^[ \t]*\}', src, re.S | re.M)
    media = re.search(r'^[ \t]*:root:not\(\[data-theme="dark"\]\) \{\n(.*?)^[ \t]*\}', src, re.S | re.M)
    if not dark:
        return [f"{path}: no :root token block"]
    if not light:
        return [f"{path}: no :root[data-theme=\"light\"] block"]
    if not media:
        return [f"{path}: no prefers-color-scheme:light block"]

    d, l, m = tokens(dark.group(1)), tokens(light.group(1)), tokens(media.group(1))

    if l != m:
        only_light = sorted(set(l) - set(m))
        only_media = sorted(set(m) - set(l))
        differing = sorted(k for k in set(l) & set(m) if l[k].strip() != m[k].strip())
        fails.append(f"{path}: the two light blocks have drifted")
        if only_light:
            fails.append(f"    missing from the media block: {', '.join(only_light)}")
        if only_media:
            fails.append(f"    missing from the explicit block: {', '.join(only_media)}")
        for k in differing:
            fails.append(f"    {k}: explicit={l[k].strip()!r} media={m[k].strip()!r}")

    # A token whose value is itself a var() is an alias; it inherits whatever the
    # token it points at resolves to, so it needs no light counterpart.
    missing_light = sorted(k for k, v in d.items()
                           if k not in NON_COLOUR and k not in l
                           and not v.strip().startswith("var("))
    if missing_light:
        fails.append(f"{path}: dark tokens with no light counterpart "
                     f"(they would keep a dark-ground value on a light ground): "
                     f"{', '.join(missing_light)}")

    # Strip comments before scanning: prose explaining the palette legitimately
    # names hexes, and a naive line test flags it.
    style = src[src.index("<style>"): src.index("</style>")]
    style = re.sub(r"/\*.*?\*/", "", style, flags=re.S)
    for line in style.split("\n"):
        st = line.strip()
        if st.startswith("--") or not st:
            continue
        if re.search(r"#[0-9a-fA-F]{3,6}\b|rgba?\(", st):
            fails.append(f"{path}: literal colour in a rule (cannot flip theme) — {st[:90]}")
    return fails


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    fails = []
    for rel in ASSETS:
        p = root / rel
        if not p.exists():
            fails.append(f"{rel}: missing")
            continue
        fails += check(p)
    if fails:
        print("check-viz-theme: FAIL")
        for f in fails:
            print(" ", f)
        return 1
    print(f"check-viz-theme: ok ({len(ASSETS)} assets, light blocks in sync)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
