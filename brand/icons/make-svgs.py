#!/usr/bin/env python3
"""Writes the Nova app icon SVGs into this folder, three crops of each:

  <app>.svg         the macOS icon grid: an 824 px rounded tile with a shadow on a 1024 px canvas (Mac app bundles)
  <app>-tile.svg    the rounded tile alone, no margin or shadow (favicons, logos, Windows and Linux icons)
  <app>-square.svg  the tile full bleed with square corners (home-screen and manifest icons; the OS rounds them)

Every icon shares the shell: a two-stop gradient in the app's colour, a light wash from the top left and the
four-point Nova sparkle. Only the colours and the glyph change.
"""
import math
from pathlib import Path

HERE = Path(__file__).resolve().parent


def sparkle(cx, cy, r, w):
    """A four-point star: long points of radius r, waist w."""
    return (f"M{cx} {cy - r} C{cx + w * 0.25} {cy - w} {cx + w} {cy - w * 0.25} {cx + r} {cy} "
            f"C{cx + w} {cy + w * 0.25} {cx + w * 0.25} {cy + w} {cx} {cy + r} "
            f"C{cx - w * 0.25} {cy + w} {cx - w} {cy + w * 0.25} {cx - r} {cy} "
            f"C{cx - w} {cy - w * 0.25} {cx - w * 0.25} {cy - w} {cx} {cy - r}Z")


def shell(name, top, bottom, glyph, crop="grid"):
    rx = 0 if crop == "square" else 185
    view = "0 0 1024 1024" if crop == "grid" else "100 100 824 824"
    size = 1024 if crop == "grid" else 824
    shadow = ' filter="url(#shadow)"' if crop == "grid" else ""
    rim = ("" if crop == "square" else
           '  <rect x="102" y="102" width="820" height="820" rx="183" fill="none" stroke="#fff" stroke-opacity="0.18" '
           'stroke-width="4"/>\n')
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="{view}">
  <title>{name}</title>
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{top}"/>
      <stop offset="1" stop-color="{bottom}"/>
    </linearGradient>
    <radialGradient id="wash" cx="0.22" cy="0.12" r="0.9">
      <stop offset="0" stop-color="#fff" stop-opacity="0.32"/>
      <stop offset="0.55" stop-color="#fff" stop-opacity="0"/>
    </radialGradient>
    <linearGradient id="ink" gradientUnits="userSpaceOnUse" x1="0" y1="240" x2="0" y2="820">
      <stop offset="0" stop-color="#ffffff"/>
      <stop offset="1" stop-color="#e6ecf5"/>
    </linearGradient>
    <filter id="shadow" x="-20%" y="-20%" width="140%" height="140%">
      <feDropShadow dx="0" dy="12" stdDeviation="14" flood-color="#000" flood-opacity="0.28"/>
    </filter>
    <filter id="lift" x="-20%" y="-20%" width="140%" height="140%">
      <feDropShadow dx="0" dy="10" stdDeviation="12" flood-color="{bottom}" flood-opacity="0.55"/>
    </filter>
    <clipPath id="tile"><rect x="100" y="100" width="824" height="824" rx="{rx}"/></clipPath>
  </defs>
  <rect x="100" y="100" width="824" height="824" rx="{rx}" fill="url(#bg)"{shadow}/>
  <g clip-path="url(#tile)">
    <rect x="100" y="100" width="824" height="824" fill="url(#wash)"/>
    <path d="{sparkle(258, 252, 62, 14)}" fill="#fff" opacity="0.9"/>
    <circle cx="258" cy="252" r="34" fill="#fff" opacity="0.18"/>
  </g>
{rim}  <g filter="url(#lift)" fill="url(#ink)" stroke="url(#ink)">
{glyph}
  </g>
</svg>
"""


def helm():
    """A ship's wheel: rim, hub and eight spokes ending in handles."""
    cx, cy = 512, 540
    parts = []
    for i in range(8):
        a = math.radians(i * 45 + 22.5)
        x1, y1 = cx + 70 * math.cos(a), cy + 70 * math.sin(a)
        x2, y2 = cx + 268 * math.cos(a), cy + 268 * math.sin(a)
        hx, hy = cx + 296 * math.cos(a), cy + 296 * math.sin(a)
        parts.append(f'    <line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" stroke-width="34" stroke-linecap="round" fill="none"/>')
        parts.append(f'    <circle cx="{hx:.1f}" cy="{hy:.1f}" r="34" stroke="none"/>')
    parts.append(f'    <circle cx="{cx}" cy="{cy}" r="196" fill="none" stroke-width="46"/>')
    parts.append(f'    <circle cx="{cx}" cy="{cy}" r="82" stroke="none"/>')
    parts.append(f'    <circle cx="{cx}" cy="{cy}" r="30" fill="#0b4fd0" stroke="none" opacity="0.55"/>')
    return "\n".join(parts)


def guard():
    """A shield with a check cut out of it."""
    return ('    <path stroke="none" fill-rule="evenodd" d="M512 248 C590 296 680 318 766 318 C766 520 718 690 512 812 '
            'C306 690 258 520 258 318 C344 318 434 296 512 248Z '
            'M402 532 L376 558 L478 660 L662 476 L636 450 L478 608Z"/>')


def link():
    """Two screens with a cursor crossing from one to the other."""
    screens = []
    for x in (196, 532):
        screens.append(f'    <rect x="{x}" y="356" width="296" height="208" rx="30" fill="none" stroke-width="34"/>')
        screens.append(f'    <rect x="{x + 128}" y="580" width="40" height="70" stroke="none"/>')
        screens.append(f'    <rect x="{x + 70}" y="640" width="156" height="34" rx="17" stroke="none"/>')
    return ("\n".join(screens) + "\n"
            '    <path stroke="none" stroke-linejoin="round" d="M488 420 L488 616 L534 572 L566 642 L600 626 L568 558 L632 554Z"/>')


def gear():
    """A key light: a soft panel on a pole with light rays."""
    rays = []
    for ang in (-150, -120, -90, -60, -30):
        a = math.radians(ang)
        x1, y1 = 512 + 230 * math.cos(a), 470 + 150 * math.sin(a) - 0
        x2, y2 = 512 + 300 * math.cos(a), 470 + 210 * math.sin(a)
        rays.append(f'    <line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" stroke-width="30" stroke-linecap="round"/>')
    return ("\n".join(rays) + "\n"
            '    <rect x="312" y="420" width="400" height="236" rx="44" stroke="none"/>\n'
            '    <rect x="350" y="456" width="324" height="164" rx="26" fill="#ffe7a8" stroke="none"/>\n'
            '    <rect x="490" y="652" width="44" height="130" rx="10" stroke="none"/>\n'
            '    <rect x="402" y="768" width="220" height="40" rx="20" stroke="none"/>')


def compute():
    """A chip with pins and a bolt on its face."""
    pins = []
    for i in range(4):
        o = 392 + i * 80
        for (x1, y1, x2, y2) in ((o, 300, o, 250), (o, 724, o, 774), (300, o + 28, 250, o + 28), (724, o + 28, 774, o + 28)):
            pins.append(f'    <line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke-width="30" stroke-linecap="round"/>')
    return ("\n".join(pins) + "\n"
            '    <rect x="300" y="300" width="424" height="424" rx="64" stroke="none"/>\n'
            '    <path stroke="none" fill="#c2185b" opacity="0.85" d="M550 368 L424 540 L506 540 L474 680 L604 496 L522 496Z"/>')


ICONS = {
    "novahelm": ("NovaHelm", "#3d9bff", "#0a3fb8", helm()),
    "novaguard": ("NovaGuard", "#34d17a", "#0b6e47", guard()),
    "novalink": ("NovaLink", "#b77cff", "#4b23b5", link()),
    "novagear": ("NovaGear", "#ffbf47", "#e8590c", gear()),
    "novacompute": ("NovaCompute", "#ff5c8a", "#a3124a", compute()),
}

if __name__ == "__main__":
    for slug, (name, top, bottom, glyph) in ICONS.items():
        for crop, suffix in (("grid", ""), ("tile", "-tile"), ("square", "-square")):
            (HERE / f"{slug}{suffix}.svg").write_text(shell(name, top, bottom, glyph, crop))
        print(f"wrote {slug}.svg, {slug}-tile.svg, {slug}-square.svg")
