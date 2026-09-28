#!/usr/bin/env python3
"""Turn a `tmux capture-pane -e -p` dump into a standalone HTML page that looks like the terminal.

Every line becomes a fixed-height block and every styled run an inline-block span, so row
backgrounds (header, selection, bars) fill the whole line height without gaps.
"""
import html
import re
import sys

FG, BG = "#d6dbe3", "#16191e"
# xterm base 16 colours, as used by 30-37 / 90-97 and 38;5;0-15
BASE16 = [
    "#000000", "#cd3131", "#0dbc79", "#e5e510", "#2472c8", "#bc3fbc", "#11a8cd", "#e5e5e5",
    "#666666", "#f14c4c", "#23d18b", "#f5f543", "#3b8eea", "#d670d6", "#29b8db", "#ffffff",
]
SGR = re.compile(r"\x1b\[([0-9;:]*)m")


def xterm256(n):
    if n < 16:
        return BASE16[n]
    if n < 232:
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (steps[n // 36], steps[n // 6 % 6], steps[n % 6])
    v = 8 + (n - 232) * 10
    return "#%02x%02x%02x" % (v, v, v)


def apply(codes, st):
    nums = [int(c) if c else 0 for c in codes.replace(":", ";").split(";")] if codes else [0]
    i = 0
    while i < len(nums):
        c = nums[i]
        if c == 0:
            st.update(fg=None, bg=None, bold=False, italic=False, reverse=False)
        elif c == 1:
            st["bold"] = True
        elif c == 3:
            st["italic"] = True
        elif c == 7:
            st["reverse"] = True
        elif c == 22:
            st["bold"] = False
        elif c == 23:
            st["italic"] = False
        elif c == 27:
            st["reverse"] = False
        elif 30 <= c <= 37:
            st["fg"] = BASE16[c - 30]
        elif 90 <= c <= 97:
            st["fg"] = BASE16[c - 90 + 8]
        elif 40 <= c <= 47:
            st["bg"] = BASE16[c - 40]
        elif 100 <= c <= 107:
            st["bg"] = BASE16[c - 100 + 8]
        elif c == 39:
            st["fg"] = None
        elif c == 49:
            st["bg"] = None
        elif c in (38, 48) and i + 1 < len(nums):
            key = "fg" if c == 38 else "bg"
            if nums[i + 1] == 2 and i + 4 < len(nums):
                st[key] = "#%02x%02x%02x" % tuple(nums[i + 2 : i + 5])
                i += 4
            elif nums[i + 1] == 5 and i + 2 < len(nums):
                st[key] = xterm256(nums[i + 2])
                i += 2
        i += 1


def css(st):
    fg, bg = st["fg"] or FG, st["bg"]
    if st["reverse"]:
        fg, bg = bg or BG, fg
    parts = [f"color:{fg}"]
    if bg:
        parts.append(f"background:{bg}")
    if st["bold"]:
        parts.append("font-weight:700")
    if st["italic"]:
        parts.append("font-style:italic")
    return ";".join(parts)


def convert(text, width):
    st = dict(fg=None, bg=None, bold=False, italic=False, reverse=False)
    out = []
    for line in text.rstrip("\n").split("\n"):
        spans, pos, cells = [], 0, 0
        for m in list(SGR.finditer(line)) + [None]:
            chunk = line[pos : m.start() if m else len(line)]
            if chunk:
                spans.append(f'<span style="{css(st)}">{html.escape(chunk)}</span>')
                cells += len(chunk)
            if m:
                apply(m.group(1), st)
                pos = m.end()
        if cells < width:
            # pad so a background that runs to the edge (header, selection) stays full width
            spans.append(f'<span style="{css(st)}">{" " * (width - cells)}</span>')
        out.append(f'<div class="l">{"".join(spans)}</div>')
    return "\n".join(out)


def main():
    width = int(sys.argv[1]) if len(sys.argv) > 1 else 150
    body = convert(sys.stdin.read(), width)
    print(f"""<!doctype html>
<html><head><meta charset="utf-8"><title>apptop</title>
<style>
  html, body {{ margin: 0; background: #0d0f12; }}
  .term {{ display: inline-block; margin: 24px; padding: 14px 16px; background: {BG};
          border-radius: 10px; box-shadow: 0 8px 30px rgba(0,0,0,.45); }}
  .l {{ height: 20px; white-space: pre; font: 14px/20px "Cascadia Mono", "DejaVu Sans Mono", monospace; }}
  .l span {{ display: inline-block; height: 20px; vertical-align: top; }}
</style></head>
<body><div class="term">
{body}
</div></body></html>""")


if __name__ == "__main__":
    main()
