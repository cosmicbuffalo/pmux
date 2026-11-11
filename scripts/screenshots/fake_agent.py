"""A stand-in for `claude` or `codex` in the README demo.

    fake_agent.py DIR PROGRAM [PROMPT...]

Run with no prompt (typed at a prompt, as `codex`), it draws DIR/idle-PROGRAM
and reads a line from the terminal, the way the agent waits for its first
prompt. Given a prompt (as `claude "do the thing"`), it skips that. Either way
it then reports the prompt to scripts/screenshots.py by writing
DIR/prompts/PANE.json, and from then on draws whatever screen the script puts
in DIR/screens/PANE, redrawing each time that file changes. PANE is the tmux
pane id without its `%`.

Screens are laid out for the pane's width, and laid out again when the pane
is resized, as a real TUI would: long lines wrap at spaces, continuing under
the text after their bullet, and lines between `[box]` (or `[box COLOR]`, a
256-color number) and `[/box]` are drawn inside a rounded box as wide as the
pane, like Claude Code's prompt and dialogs.

The wrapper runs this with a symlink to python3 named PROGRAM, so tmux, and
pmux through it, see the pane running `claude` or `codex`.
"""

import json
import os
import re
import signal
import sys
import time
from pathlib import Path

CLEAR = "\033[?25l\033[2J\033[H"
RESET = "\033[0m"
ESCAPE = re.compile(r"\033\[[0-9;?]*[A-Za-z]")
BOX = re.compile(r"^\[box(?: (\d+))?\]$")
# Glyphs a continuation line is indented past, as agents indent wrapped text.
BULLETS = "●✶✻•>└❯▌"


def width():
    try:
        return os.get_terminal_size(sys.stdout.fileno()).columns
    except OSError:
        return 80


def tokens(line):
    """`line` as escape sequences and single visible characters."""
    out, at = [], 0
    for match in ESCAPE.finditer(line):
        out += list(line[at:match.start()])
        out.append(match.group())
        at = match.end()
    return out + list(line[at:])


def visible(token):
    return not token.startswith("\033")


def styles(items):
    """The SGR sequences still in effect after `items`."""
    active = []
    for token in items:
        if token.startswith("\033[") and token.endswith("m"):
            active = [] if token in (RESET, "\033[m") else active + [token]
    return active


def indent_of(items):
    """Columns a wrapped line continues at: past leading spaces and a bullet."""
    text = "".join(t for t in items if visible(t))
    stripped = text.lstrip(" ")
    indent = len(text) - len(stripped)
    if stripped[:1] in BULLETS:
        indent += 1 + len(stripped[1:]) - len(stripped[1:].lstrip(" "))
    return indent


def wrap(line, limit):
    """`line` broken at spaces into lines of at most `limit` columns."""
    items = tokens(line)
    if sum(map(visible, items)) <= limit:
        return [line]
    indent = indent_of(items)
    lines = []
    while True:
        columns, space, cut = 0, None, len(items)
        for index, token in enumerate(items):
            if not visible(token):
                continue
            if columns >= limit:
                cut = index
                break
            if token == " " and columns > indent:
                space = index
            columns += 1
        if cut == len(items):
            lines.append("".join(items))
            return lines
        if space is not None:
            head, rest = items[:space], items[space + 1:]
        else:
            head, rest = items[:cut], items[cut:]
        lines.append("".join(head) + RESET)
        while rest and rest[0] == " ":
            rest = rest[1:]
        items = list(" " * indent) + styles(head) + rest


def box(color, content, columns):
    """`content` in a rounded box spanning `columns`, one space from each edge."""
    border = f"\033[38;5;{color}m" if color else "\033[2m"
    inner = max(columns - 6, 10)
    rows = [f" {border}╭{'─' * (inner + 2)}╮{RESET}"]
    for line in content:
        for part in wrap(line, inner):
            pad = inner - sum(map(visible, tokens(part)))
            rows.append(f" {border}│{RESET} {part}{RESET}{' ' * pad} {border}│{RESET}")
    rows.append(f" {border}╰{'─' * (inner + 2)}╯{RESET}")
    return rows


def layout(screen, columns):
    """`screen` fitted to `columns`: wrapped lines and pane-wide boxes."""
    rows, boxed = [], None
    for line in screen.split("\n"):
        opening = BOX.match(line.strip())
        if opening:
            boxed = (opening.group(1), [])
        elif line.strip() == "[/box]" and boxed:
            rows += box(*boxed, columns)
            boxed = None
        elif boxed:
            boxed[1].append(line)
        else:
            rows += wrap(line, columns - 1)
    return "\n".join(rows)


def draw(screen):
    sys.stdout.write(CLEAR + layout(screen, width()))
    sys.stdout.flush()


def main():
    directory, program, *prompt = sys.argv[1:]
    directory = Path(directory)
    pane = os.environ.get("TMUX_PANE", "%0").lstrip("%")
    prompt = " ".join(prompt)
    if not prompt:
        idle = directory / f"idle-{program}"
        draw(idle.read_text() if idle.exists() else "")
        try:
            prompt = input().strip()
        except EOFError:
            prompt = ""
        draw("")
    report = directory / "prompts" / f"{pane}.json"
    report.parent.mkdir(parents=True, exist_ok=True)
    partial = report.with_suffix(".tmp")
    partial.write_text(json.dumps({"pane": f"%{pane}", "program": program,
                                   "cwd": os.getcwd(), "prompt": prompt}))
    partial.rename(report)

    # From here on, redraw when the screen changes or the pane is resized.
    resized = []
    signal.signal(signal.SIGWINCH, lambda *_: resized.append(True))
    screen = directory / "screens" / pane
    shown, text = None, None
    while True:
        try:
            stamp = screen.stat().st_mtime_ns
        except FileNotFoundError:
            stamp = None
        if stamp is not None and stamp != shown:
            shown, text = stamp, screen.read_text()
            resized.clear()
            draw(text)
        elif resized and text is not None:
            resized.clear()
            draw(text)
        time.sleep(0.1)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
