#!/usr/bin/env python3
"""Render the README screenshots from a real pmux setup with default config.

Every scene runs the pmux binary from this checkout against throwaway tmux
servers: an "outer" tmux with only pmux's generated integration sourced, and
pmux's own backend with its generated default config. A few fake agents
(`claude` and `codex` are renamed copies of `sleep` that first print a static
screen), their transcripts, and ai-attn state records give the sidebar,
picker, and status bars something realistic to show. VHS
(https://github.com/charmbracelet/vhs) attaches a real terminal to the outer
server and saves docs/images/SCENE.png.

    python3 scripts/screenshots.py                 # every scene
    python3 scripts/screenshots.py sidebar         # just the named scenes
    python3 scripts/screenshots.py demo            # the README demo recording
    python3 scripts/screenshots.py customization   # every example in the doc
    python3 scripts/screenshots.py customization/keys

The demo scene records docs/images/demo.gif (and target/screenshots/demo.mp4
for a release) while a timeline thread changes agent states and spawns a
helper, as hooks and agents would.

Each example in docs/sidebar-customization.md, marked by a
`<!-- example: NAME -->` line before its toml block, is also a scene: it
writes that block as the config.toml, shows it as DOC_EXAMPLES says, and
saves docs/images/customization/NAME.png.

Needs tmux, vhs, ttyd, ffmpeg, ruby with tmux.rb (the tmux gem, or
TMUX_RB=/path/to/tmux.rb), and ai-attn (for the attention states). Builds pmux
first. The font is downloaded once, checksum-verified, into
target/screenshots and exposed to VHS's browser through a private fontconfig
file. pmux's own bootstrap downloads its pinned tmux-ai-attn, as it does for
users. Nothing touches a
running tmux server, your pmux processes, or your real home directory: every
server, socket, and file lives in a temporary directory.
"""

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUTPUT_DIR = ROOT / "docs/images"
CACHE_DIR = ROOT / "target/screenshots"
BINARY = ROOT / "target/release/pmux"

FONT = {
    "url": "https://github.com/ryanoasis/nerd-fonts/releases/download/v3.5.1/JetBrainsMono.tar.xz",
    "sha256": "04d5e8f903693f9dd13e16f867e994834e681eb3c72c0d337a770dcda09010cf",
    "family": "JetBrainsMono Nerd Font Mono",
    "files": [
        "JetBrainsMonoNerdFontMono-Regular.ttf",
        "JetBrainsMonoNerdFontMono-Bold.ttf",
        "JetBrainsMonoNerdFontMono-Italic.ttf",
        "OFL.txt",
    ],
}
TERMINAL = {"theme": "Catppuccin Mocha", "font_size": 16, "padding": 14}

# Programs a pane can appear to run. Scene scripts draw a screen and then run
# the program as a renamed copy of sleep, so tmux reports it as the pane's
# command and pmux recognizes the agent. Typed at a prompt (as in the demo),
# the program is FAKE_AGENT instead, named the same way.
FAKE_PROGRAMS = ["claude", "codex", "node"]

# The tmux that runs pmux in the scenes: a Catppuccin theme with pmux's
# integration and window tabs from its tmux.rb module, compiled with tmux.rb.
OUTER_SOURCE = ROOT / "scripts/screenshots/outer.rb"
# What a typed `claude` or `codex` runs: it waits for a prompt, then draws the
# screens the demo hands it (see the file's docstring).
FAKE_AGENT = ROOT / "scripts/screenshots/fake_agent.py"

CLAUDE_SCREEN = """\

 \033[38;5;216m✻\033[0m Welcome back!

 \033[1m> refactor the session loader to stream transcripts\033[0m

 \033[38;5;216m●\033[0m I'll start by reading the current loader.

 \033[38;5;150m●\033[0m Read(src/session/loader.rs)
   └  Read 214 lines

 \033[38;5;150m●\033[0m Update(src/session/loader.rs)
   └  Updated src/session/loader.rs with 38 additions and 52 removals

 \033[38;5;216m●\033[0m The loader now reads each transcript line by line instead
   of buffering the whole file. Running the tests next.

 \033[38;5;216m✶\033[0m Running cargo test… \033[2m(esc to interrupt)\033[0m

 \033[2m╭──────────────────────────────────────────────────────────╮\033[0m
 \033[2m│\033[0m >                                                        \033[2m│\033[0m
 \033[2m╰──────────────────────────────────────────────────────────╯\033[0m
"""

CODEX_SCREEN = """\

 \033[1m>_ codex\033[0m

 \033[1muser\033[0m
 add rate limiting to the upload endpoint

 \033[1mcodex\033[0m
 Added a token-bucket limiter in front of the upload handler and
 covered the burst and refill cases with tests.

 \033[2m▌ Ask codex to do anything\033[0m
"""

PERMISSION_SCREEN = """\

 \033[38;5;216m✻\033[0m Welcome back!

 \033[1m> add dark mode to the settings page\033[0m

 \033[38;5;150m●\033[0m Update(web/src/settings/theme.ts)
   └  Added a dark palette and a toggle bound to the system setting

 \033[38;5;216m●\033[0m Bash(npm install --save-dev @testing-library/user-event)

 \033[38;5;216m╭──────────────────────────────────────────────────────────╮\033[0m
 \033[38;5;216m│\033[0m \033[1mBash command\033[0m                                             \033[38;5;216m│\033[0m
 \033[38;5;216m│\033[0m   npm install --save-dev @testing-library/user-event     \033[38;5;216m│\033[0m
 \033[38;5;216m│\033[0m                                                          \033[38;5;216m│\033[0m
 \033[38;5;216m│\033[0m Do you want to proceed?                                  \033[38;5;216m│\033[0m
 \033[38;5;216m│\033[0m \033[38;5;216m❯ 1. Yes\033[0m                                                 \033[38;5;216m│\033[0m
 \033[38;5;216m│\033[0m   2. No, and tell Claude what to do differently          \033[38;5;216m│\033[0m
 \033[38;5;216m╰──────────────────────────────────────────────────────────╯\033[0m
"""

TESTS_SCREEN = """\

 \033[38;5;216m✻\033[0m Spawned by loader-refactor

 \033[1m> fix the loader tests that broke in the streaming change\033[0m

 \033[38;5;150m●\033[0m Bash(cargo test session::loader)
   └  2 failed: partial_line_is_buffered, empty_file_yields_nothing

 \033[38;5;216m✶\033[0m Reading the failing tests… \033[2m(esc to interrupt)\033[0m
"""

NODE_SCREEN = """\
 \033[1mVITE\033[0m v6.2.0  ready in 312 ms

 ➜  Local:   http://localhost:5173/
 ➜  Network: use --host to expose
"""

# Live processes, newest first: the picker's order. The sidebar lists the same
# start order with agents at work (or waiting on the user) moved to the top.
# `name: None` lets pmux generate one, so the sidebar shows the agent's
# conversation title instead.
PROCESSES = [
    {"name": None, "program": "codex", "project": "api", "screen": CODEX_SCREEN,
     "title": "Add rate limiting to the upload endpoint", "state": "done",
     "reason": "agent-turn-complete", "age": 140},
    {"name": None, "program": "claude", "project": "web", "screen": PERMISSION_SCREEN,
     "title": "Add dark mode to the settings page", "state": "waiting",
     "reason": "permission_prompt", "age": 95},
    {"name": "dev-server", "program": "node", "project": "web", "screen": NODE_SCREEN},
    {"name": "loader-refactor", "program": "claude", "project": "api",
     "screen": CLAUDE_SCREEN, "title": "Stream transcripts in the session loader",
     "state": "working", "reason": "", "age": 20},
]

# Past conversations for the sidebar's Resumable group and the picker.
PAST_SESSIONS = [
    ("api", "Migrate CI to Ubuntu 24.04", 3 * 3600),
    ("web", "Design the settings page", 26 * 3600),
    ("docs", "Write the getting started guide", 50 * 3600),
]

SCENES = {}

CUSTOMIZATION_DOC = ROOT / "docs/sidebar-customization.md"
# How each example in the customization doc is shown: in the sidebar, the
# picker, or the sidebar's keymap.
DOC_EXAMPLES = {
    "preset-pmux": "sidebar",
    "preset-compact": "sidebar",
    "preset-herdr": "sidebar",
    "lines": "sidebar",
    "colors": "sidebar",
    "outline-rounded": "sidebar",
    "outline-glyphs": "sidebar",
    "picker-compact": "picker",
    "keys": "keymap",
}
# The sidebar's corner of a full-window shot, in pixels, through the end of
# its footer.
SIDEBAR_CROP = (448, 650)


def scene(fn):
    SCENES[fn.__name__] = fn
    return fn


def run(cmd, env=None, check=True, **kwargs):
    result = subprocess.run(cmd, env=env, text=True, capture_output=True, **kwargs)
    if check and result.returncode != 0:
        sys.exit(f"command failed: {' '.join(map(str, cmd))}\n{result.stderr}{result.stdout}")
    return result


def require_tools():
    missing = [t for t in ["tmux", "vhs", "ttyd", "ffmpeg", "ruby", "cargo"] if not shutil.which(t)]
    if missing:
        sys.exit(f"missing tools: {', '.join(missing)}")
    if not shutil.which("ai-attn"):
        print("warning: ai-attn is not installed; agent states will not show", file=sys.stderr)


def font_config():
    fonts = CACHE_DIR / "fonts"
    if not all((fonts / f).exists() for f in FONT["files"]):
        fonts.mkdir(parents=True, exist_ok=True)
        archive = CACHE_DIR / Path(FONT["url"]).name
        run(["curl", "-sSLf", "-o", str(archive), FONT["url"]])
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        if digest != FONT["sha256"]:
            sys.exit(f"font checksum mismatch: {digest}")
        run(["tar", "-xJf", str(archive), "-C", str(fonts), *FONT["files"]])
    config = CACHE_DIR / "fonts.conf"
    config.write_text(textwrap.dedent(f"""\
        <?xml version="1.0"?>
        <!DOCTYPE fontconfig SYSTEM "fonts.dtd">
        <fontconfig>
          <include ignore_missing="yes">/etc/fonts/fonts.conf</include>
          <dir>{fonts}</dir>
          <cachedir>{CACHE_DIR / 'fontcache'}</cachedir>
        </fontconfig>
    """))
    return config


def compile_outer_config():
    """outer.rb compiled with tmux.rb: TMUX_RB=/path/to/tmux.rb or the tmux gem."""
    load_path = ["-I", str(Path(os.environ["TMUX_RB"]) / "lib")] if os.environ.get("TMUX_RB") else []
    code = 'require "tmux"; print Tmux.load_file(ARGV[0]).to_tmux'
    return run(["ruby", *load_path, "-e", code, str(OUTER_SOURCE)]).stdout


class World:
    """An isolated home, fake agents, and the two tmux servers."""

    def __init__(self, directory):
        self.dir = Path(directory)
        self.home = self.dir / "home/demo"
        self.code = self.home / "code"
        for project in ["api", "web", "docs"]:
            (self.code / project).mkdir(parents=True)
        self.bin = self.dir / "bin"
        self.bin.mkdir()
        sleep = shutil.which("sleep")
        real = self.bin / "real"
        real.mkdir()
        # Where typed agents (FAKE_AGENT) and the demo talk to each other.
        self.fake = self.dir / "fake"
        (self.fake / "screens").mkdir(parents=True)
        for program, screen in IDLE_SCREENS.items():
            (self.fake / f"idle-{program}").write_text(screen)
        # python3 under each program's name, so tmux sees `claude`, not python3.
        named = self.bin / "named"
        named.mkdir()
        for name in FAKE_PROGRAMS:
            shutil.copy(sleep, real / name)
            os.symlink(sys.executable, named / name)
            wrapper = self.bin / name
            wrapper.write_text(textwrap.dedent(f"""\
                #!/bin/bash
                # pmux starts codex with --no-daemon; neither stand-in takes flags.
                [ "$1" = "--no-daemon" ] && shift
                # Scene scripts run `NAME 86400` after drawing their own screen.
                if [ $# -eq 1 ] && [[ $1 =~ ^[0-9]+$ ]]; then
                  exec "{real / name}" "$1"
                fi
                exec "{named / name}" -I "{FAKE_AGENT}" "{self.fake}" {name} "$@"
            """))
            wrapper.chmod(0o755)
        (self.home / ".local/bin").mkdir(parents=True)
        os.symlink(BINARY, self.home / ".local/bin/pmux")
        # Short: tmux keeps its sockets below TMUX_TMPDIR.
        self.tmux_tmp = Path(tempfile.mkdtemp(prefix="pmux-shot-", dir="/tmp"))
        self.outer_socket = self.dir / "outer.sock"
        self.env = {
            "HOME": str(self.home),
            "PATH": os.pathsep.join([str(self.bin), str(self.home / ".local/bin"), os.environ["PATH"]]),
            "TMUX_TMPDIR": str(self.tmux_tmp),
            "XDG_RUNTIME_DIR": str(self.dir / "run"),
            "TERM": "xterm-256color",
            "LANG": "C.UTF-8",
            "USER": "demo",
            # Scenes show pmux with only the add-ons this script fakes; keep
            # an ai-auto installed on this machine out of them.
            "AI_AUTO_BIN": "ai-auto-not-installed",
        }
        (self.dir / "run").mkdir(mode=0o700)
        self.pmux_socket = self.tmux_tmp / f"tmux-{os.getuid()}" / "pmux"

    def cmd(self, *args, check=True):
        return run(list(args), env=self.env, check=check, cwd=self.home)

    def pmux_tmux(self, *args, check=True):
        return self.cmd("tmux", "-L", "pmux", *args, check=check)

    def outer(self, *args, check=True):
        return self.cmd("tmux", "-S", str(self.outer_socket), *args, check=check)

    def screen_script(self, name, program, screen):
        script = self.dir / f"{name}.sh"
        body = screen.replace("'", "'\\''")
        script.write_text(
            f"printf '\\033[?25l\\033[2J\\033[H'\nprintf '%b' '{body}'\n"
            f"exec {self.bin / program} 86400\n"
        )
        return str(script)

    def write_transcript(self, project, session_id, title, age):
        cwd = self.code / project
        stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() - age))
        path = self.home / ".claude/projects" / str(cwd).replace("/", "-") / f"{session_id}.jsonl"
        path.parent.mkdir(parents=True, exist_ok=True)
        lines = [
            {"type": "user", "cwd": str(cwd), "gitBranch": "main", "timestamp": stamp,
             "sessionId": session_id, "message": {"role": "user", "content": title.lower()}},
            {"type": "assistant", "timestamp": stamp, "sessionId": session_id,
             "message": {"role": "assistant", "content": []}},
            {"type": "ai-title", "aiTitle": title, "sessionId": session_id},
        ]
        path.write_text("".join(json.dumps(line) + "\n" for line in lines))
        mtime = time.time() - age
        os.utime(path, (mtime, mtime))

    def write_codex_rollout(self, project, session_id, title, age):
        cwd = self.code / project
        stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() - age))
        day = time.strftime("%Y/%m/%d", time.gmtime(time.time() - age))
        path = self.home / ".codex/sessions" / day / f"rollout-{stamp.replace(':', '-')[:19]}-{session_id}.jsonl"
        path.parent.mkdir(parents=True, exist_ok=True)
        lines = [
            {"timestamp": stamp, "type": "session_meta",
             "payload": {"id": session_id, "cwd": str(cwd), "source": "cli", "thread_source": "user",
                         "git": {"branch": "main"}}},
            {"timestamp": stamp, "type": "response_item",
             "payload": {"type": "message", "role": "user",
                         "content": [{"type": "input_text", "text": title}]}},
        ]
        path.write_text("".join(json.dumps(line) + "\n" for line in lines))

    def start(self, processes=None):
        # pmux writes its default config on first use; the outer server then
        # sources the generated integration.
        self.cmd("pmux", "init")
        self.processes = []
        # Both views order processes by tmux's session_created, which has
        # one-second resolution. Start them bottom first, each in a later second
        # than the last, so the order never depends on where a second boundary
        # happens to fall.
        last_second = None
        processes = PROCESSES if processes is None else processes
        for index, spec in reversed(list(enumerate(processes))):
            while last_second is not None and int(time.time()) <= last_second:
                time.sleep(0.05)
            last_second = int(time.time())
            # A fixed epoch keeps generated names, and so their hashed pill
            # colors, the same on every run.
            name = spec["name"] or f"1774574118-demo{index:04d}"
            script = self.screen_script(f"p{index}", spec["program"], spec["screen"])
            self.cmd("pmux", "run", "-d", "-s", name, "--cwd", str(self.code / spec["project"]),
                     "sh", script)
            self.processes.append((name, spec))
        for project, title, age in PAST_SESSIONS:
            self.write_transcript(project, str(uuid.uuid4()), title, age)
        self.mark_agents()
        self.outer_config = self.dir / "outer.conf"
        self.outer_config.write_text(OUTER_CONFIG_TEXT)

    def mark_agents(self):
        """Give each agent a transcript, an identity, and an ai-attn record."""
        state_dir = self.home / ".local/state/ai-attn"
        state_dir.mkdir(parents=True, exist_ok=True)
        identities = self.home / ".local/state/pmux/agent_sessions"
        identities.mkdir(parents=True, exist_ok=True)
        self.records = {}
        for name, spec in self.processes:
            if spec["program"] in ("claude", "codex"):
                self.mark_agent(name, spec)
        self.cmd("pmux", "status-labels", check=False)

    def mark_agent(self, name, spec, wait_for_start=False):
        """Give one agent a transcript, an identity, and an ai-attn record."""
        state_dir = self.home / ".local/state/ai-attn"
        identities = self.home / ".local/state/pmux/agent_sessions"
        now = int(time.time())
        session_id = str(uuid.uuid4())
        if spec["program"] == "claude":
            self.write_transcript(spec["project"], session_id, spec["title"], spec["age"])
        else:
            self.write_codex_rollout(spec["project"], session_id, spec["title"], spec["age"])
        info = self.pmux_tmux("display-message", "-p", "-t", f"={name}:",
                              "#{pane_id}\t#{session_created}").stdout.strip()
        pane, created = info.split("\t")
        (identities / name).write_text(f"{spec['program']}\t{session_id}\t{created}\n")
        record = {
            "version": 1, "agent": spec["program"], "session_key": session_id[:12],
            "state": spec["state"], "reason": spec["reason"],
            "updated_at": now - spec["age"], "cwd": str(self.code / spec["project"]),
            "session_id": session_id, "pane_id": pane, "socket": str(self.pmux_socket),
        }
        # pmux ignores records from before the agent started, so move its
        # recorded start back past the record's faked age.
        start_path = self.home / ".local/state/pmux/agent_starts" / name
        # A process typed at a prompt (`pmux run claude`) records its start
        # as it launches, which may not have happened yet.
        deadline = time.time() + (3 if wait_for_start else 0)
        while not start_path.exists() and time.time() < deadline:
            time.sleep(0.05)
        if start_path.exists():
            start = json.loads(start_path.read_text())
            start["started_at"] = min(start["started_at"], record["updated_at"] - 60)
            start_path.write_text(json.dumps(start))
        path = state_dir / f"{spec['program']}-{session_id[:12]}.json"
        path.write_text(json.dumps(record))
        self.records[name] = (path, record)

    def set_agent_state(self, name, state, reason=""):
        """Move an agent to `state`, as its ai-attn hooks would."""
        path, record = self.records[name]
        record = {**record, "state": state, "reason": reason, "updated_at": int(time.time())}
        path.write_text(json.dumps(record))
        self.records[name] = (path, record)

    def spawn_agent(self, name, spec, parent):
        """Start an agent as `parent`'s child, the way an agent's `pmux run` would."""
        script = self.screen_script(name, spec["program"], spec["screen"])
        self.cmd("pmux", "run", "-d", "-s", name, "--parent", parent,
                 "--cwd", str(self.code / spec["project"]), "sh", script)
        self.processes.append((name, spec))
        self.mark_agent(name, spec)

    def start_outer(self, windows):
        """Start the outer server with `windows`: (name, shell command) pairs."""
        (first_name, first_cmd), *rest = windows
        self.outer("-f", str(self.outer_config), "new-session", "-d", "-s", "work",
                   "-n", first_name, "-x", "160", "-y", "44", first_cmd)
        for name, command in rest:
            self.outer("new-window", "-t", "work:", "-n", name, command)
        # Every window shows an agent; wait until tmux-ai-attn (downloaded on
        # first load) gives each tab its indicator.
        deadline = time.time() + 60
        while time.time() < deadline:
            states = self.outer("list-windows", "-t", "work", "-F",
                                "#{E:@ai_attn_window_segment}", check=False).stdout.split("\n")[:-1]
            if states and all(states):
                return
            time.sleep(0.5)
        print("warning: tab attention indicators did not appear", file=sys.stderr)

    def refresh_records(self):
        """Rewrite every agent record, keeping its state and its age."""
        now = int(time.time())
        ages = {name: spec.get("age", 0) for name, spec in self.processes}
        for name, (path, record) in list(self.records.items()):
            record = {**record, "updated_at": now - ages.get(name, 0)}
            path.write_text(json.dumps(record))
            self.records[name] = (path, record)
        self.cmd("pmux", "status-labels", check=False)

    def attach_command(self, name):
        return f"pmux attach {name}"

    def stop(self):
        self.outer("kill-server", check=False)
        self.pmux_tmux("kill-server", check=False)
        # pmux's attention relay and tmux-ai-attn's sync loop outlive the
        # servers; stop everything running with this scene's home.
        marker = f"HOME={self.home}".encode()
        for proc in Path("/proc").glob("[0-9]*"):
            try:
                if marker in (proc / "environ").read_bytes().split(b"\0"):
                    os.kill(int(proc.name), 15)
            except (OSError, ValueError):
                pass
        time.sleep(0.5)
        shutil.rmtree(self.tmux_tmp, ignore_errors=True)


def vhs(world, output, keys, width, height, fonts, settle="2.5s"):
    """Attach a VHS terminal to the outer server, press `keys`, and save `output`."""
    env_prefix = " ".join(f"{k}={v}" for k, v in world.env.items() if k != "PATH")
    attach = f"env {env_prefix} PATH={world.env['PATH']} tmux -S {world.outer_socket} attach -t work"
    steps = []
    for key in keys:
        # VHS commands (Ctrl+P, Enter, Sleep 1s, ...) pass through; anything
        # else is typed as text.
        special = re.match(r"^(Ctrl\+|Alt\+|Sleep |Enter|Escape|Tab|Up|Down|Left|Right)", key)
        steps.append(key if special else f'Type "{key}"')
        steps.append("Sleep 500ms")
    tape = world.dir / f"{output.stem}.tape"
    tape.write_text(textwrap.dedent(f"""\
        Output "{world.dir / (output.stem + '.gif')}"
        Set Shell "bash"
        Set FontFamily "{FONT['family']}"
        Set FontSize {TERMINAL['font_size']}
        Set Width {width}
        Set Height {height}
        Set Padding {TERMINAL['padding']}
        Set WindowBar Colorful
        Set Theme "{TERMINAL['theme']}"
        Hide
        Type "{attach}"
        Enter
        Sleep {settle}
        Show
    """) + "\n".join(steps) + f"\nSleep 1s\nScreenshot \"{output}\"\nSleep 300ms\n")
    run(["vhs", str(tape)], env={**os.environ, "FONTCONFIG_FILE": str(fonts)})


@scene
def sidebar(world, fonts):
    """The outer sidebar beside an attached agent, with tab indicators."""
    # Each window shows a different agent, so the tabs carry different
    # attention indicators: working (this window), waiting, and done.
    agents = {spec["state"]: name for name, spec in world.processes if spec.get("state")}
    world.start_outer([
        ("api", world.attach_command("loader-refactor")),
        ("web", world.attach_command(agents["waiting"])),
        ("docs", world.attach_command(agents["done"])),
    ])
    world.outer("select-window", "-t", "work:api")
    vhs(world, OUTPUT_DIR / "sidebar.png", ["Ctrl+B", "Ctrl+P", "Sleep 2s"], 1500, 700, fonts)


@scene
def picker(world, fonts):
    """The attach picker opened with prefix + s inside a process."""
    world.start_outer([("api", world.attach_command("loader-refactor"))])
    vhs(world, OUTPUT_DIR / "picker.png", ["Ctrl+P", "s", "Sleep 1.5s"], 1300, 700, fonts)


@scene
def keys(world, fonts):
    """The key hints popup opened with prefix + ? inside a process."""
    world.start_outer([("api", world.attach_command("loader-refactor"))])
    vhs(world, OUTPUT_DIR / "keys.png", ["Ctrl+P", "?", "Sleep 1.5s"], 1300, 900, fonts)


def doc_examples():
    """The customization doc's examples: {name: toml text}."""
    text = CUSTOMIZATION_DOC.read_text()
    examples = dict(re.findall(r"<!-- example: ([\w-]+) -->\n```toml\n(.*?)```", text, re.S))
    if set(examples) != set(DOC_EXAMPLES):
        sys.exit("docs/sidebar-customization.md examples and DOC_EXAMPLES differ: "
                 f"{sorted(set(examples) ^ set(DOC_EXAMPLES))}")
    return examples


def example_scene(name, view, config):
    """Show the doc example `name`: `config` as config.toml, seen in `view`."""
    def show(world, fonts):
        (world.home / ".config/pmux/config.toml").write_text(config)
        output = OUTPUT_DIR / "customization" / f"{name}.png"
        output.parent.mkdir(parents=True, exist_ok=True)
        if view == "picker":
            world.start_outer([("api", world.attach_command("loader-refactor"))])
            vhs(world, output, ["Ctrl+P", "s", "Sleep 1.5s"], 1100, 560, fonts)
            return
        agents = {spec["state"]: name for name, spec in world.processes if spec.get("state")}
        world.start_outer([
            ("api", world.attach_command("loader-refactor")),
            ("web", world.attach_command(agents["waiting"])),
            ("docs", world.attach_command(agents["done"])),
        ])
        world.outer("select-window", "-t", "work:api")
        keys = ["Ctrl+B", "Ctrl+P", "Sleep 2s"]
        if view == "keymap":
            # Into the sidebar (tmux's prefix + Left), then its keymap.
            keys += ["Ctrl+B", "Left", "?", "Sleep 1s"]
        full = world.dir / f"{name}-full.png"
        vhs(world, full, keys, 1500, 700, fonts)
        width, height = SIDEBAR_CROP
        run(["ffmpeg", "-y", "-loglevel", "error", "-i", str(full),
             "-vf", f"crop={width}:{height}:0:0", str(output)])
    return show


# The README demo (planning/demo-script.md describes it in plain English).
# It starts at an empty prompt; agents typed there run FAKE_AGENT, which shows
# IDLE_SCREENS until a prompt is typed and then whatever screen the demo gives
# it.


def at_cursor(screen, row, col):
    """`screen`, then the cursor shown at `row`, `col` (1-based), where typing lands."""
    return f"{screen}\033[{row};{col}H\033[?25h"


IDLE_SCREENS = {
    "codex": at_cursor("""\

 \033[1m>_ codex\033[0m
 \033[2mdirectory: ~/code/api\033[0m

 \033[2m▌ Ask codex to do anything\033[0m
""", 5, 4),
    "claude": at_cursor("""\

 \033[38;5;216m✻\033[0m Welcome back!
   \033[2mcwd: ~/code/web\033[0m

[box]
>
[/box]
   \033[2m? for shortcuts\033[0m
""", 6, 6),
}

# FAKE_AGENT draws [box] blocks as wide as the pane and wraps long lines, so
# the screens fit a narrow split; see its docstring.
PROMPT_BOX = """
[box]
>
[/box]
"""

DEMO_CODEX_WORKING = """\

 \033[1m>_ codex\033[0m

 \033[1muser\033[0m
 {prompt}

 \033[38;5;111m•\033[0m Reading src/routes/upload.rs

 \033[38;5;216m•\033[0m Working \033[2m(4s • esc to interrupt)\033[0m

 \033[2m▌ Ask codex to do anything\033[0m
"""

DEMO_CODEX_DONE = """\

 \033[1m>_ codex\033[0m

 \033[1muser\033[0m
 {prompt}

 \033[1mcodex\033[0m
 Added a token-bucket limiter in front of the upload handler and covered the burst and refill cases with tests.

 \033[2m▌ Ask codex to do anything\033[0m
"""

DEMO_DARK_WORKING = """\

 \033[38;5;216m✻\033[0m Welcome back!

 \033[1m> {prompt}\033[0m

 \033[38;5;216m●\033[0m I'll start with how the settings page picks its colors.

 \033[38;5;150m●\033[0m Read(web/src/settings/theme.ts)
   └  Read 88 lines

 \033[38;5;216m✶\033[0m Thinking… \033[2m(esc to interrupt)\033[0m
""" + PROMPT_BOX

DEMO_DARK_WAITING = """\

 \033[38;5;216m✻\033[0m Welcome back!

 \033[1m> {prompt}\033[0m

 \033[38;5;150m●\033[0m Update(web/src/settings/theme.ts)
   └  Added a dark palette and a theme toggle

 \033[38;5;216m●\033[0m Bash(npm install -D @testing-library/react)

[box 216]
\033[1mBash command\033[0m
  npm install -D @testing-library/react

Do you want to proceed?
\033[38;5;216m❯ 1. Yes\033[0m
  2. No, tell Claude what to do (esc)
[/box]
"""

DEMO_LOADER_WORKING = """\

 \033[38;5;216m✻\033[0m Welcome back!

 \033[1m> {prompt}\033[0m

 \033[38;5;216m●\033[0m I'll start by reading the current loader.

 \033[38;5;150m●\033[0m Read(src/session/loader.rs)
   └  Read 214 lines

 \033[38;5;150m●\033[0m Update(src/session/loader.rs)
   └  Updated src/session/loader.rs with 38 additions and 52 removals

 \033[38;5;216m✶\033[0m Running cargo test… \033[2m(esc to interrupt)\033[0m
""" + PROMPT_BOX

DEMO_LOADER_DONE = """\

 \033[38;5;216m✻\033[0m Welcome back!

 \033[1m> {prompt}\033[0m

 \033[38;5;150m●\033[0m Update(src/session/loader.rs)
   └  Updated src/session/loader.rs with 38 additions and 52 removals

 \033[38;5;150m●\033[0m Bash(cargo test session::loader)
   └  48 passed

 \033[38;5;216m●\033[0m The loader now streams each transcript line by line, and a helper fixed the two tests that assumed a buffered file.
""" + PROMPT_BOX

# The typed agents, by program and the project they start in. `title` is the
# label pmux shows (Claude's generated conversation title; Codex falls back to
# the first prompt), and `screens` what the agent draws in each state.
DEMO_AGENTS = {
    ("codex", "api"): {"name": None, "program": "codex", "project": "api",
                       "title": "Add rate limiting to the upload endpoint",
                       "state": "working", "reason": "", "age": 0,
                       "screens": {"working": DEMO_CODEX_WORKING, "done": DEMO_CODEX_DONE}},
    ("claude", "web"): {"name": None, "program": "claude", "project": "web",
                        "title": "Add dark mode to the settings page",
                        "state": "working", "reason": "", "age": 0,
                        "screens": {"working": DEMO_DARK_WORKING, "waiting": DEMO_DARK_WAITING}},
    ("claude", "api"): {"name": "loader-refactor", "program": "claude", "project": "api",
                        "title": "Stream transcripts in the session loader",
                        "state": "working", "reason": "", "age": 0,
                        "screens": {"working": DEMO_LOADER_WORKING, "done": DEMO_LOADER_DONE}},
}
DEMO_HELPER = "1774574118-demo9000"
DEMO_HELPER_SPEC = {"name": None, "program": "claude", "project": "api", "screen": TESTS_SCREEN,
                    "title": "Fix the loader tests broken by streaming", "state": "working",
                    "reason": "", "age": 0}
DEMO_PROMPT = r"\[\e[38;5;150m\]demo\[\e[0m\] \[\e[38;5;111m\]\w\[\e[0m\] $ "
# How long after a prompt is entered the agent's label appears, as a title
# being generated.
LABEL_DELAY = 1.0

# What is typed and pressed, as VHS commands. Agents start working when their
# prompt is entered; DEMO_EVENTS take over once the sidebar opens.
DEMO_KEYS = """\
    Sleep 1.5s
    Type@60ms "pmux wrap codex"
    Sleep 500ms
    Enter
    Sleep 1.5s
    Type@60ms "codex"
    Sleep 500ms
    Enter
    Sleep 2s
    Type@45ms "Add rate limiting to the upload endpoint"
    Sleep 500ms
    Enter
    Sleep 3s
    Ctrl+P
    Sleep 1s
    Type "d"
    Sleep 2s
    Type@60ms "pmux wrap claude"
    Sleep 500ms
    Enter
    Sleep 1.5s
    Type@60ms "cd ../web"
    Sleep 400ms
    Enter
    Sleep 600ms
    Type@60ms "claude"
    Sleep 500ms
    Enter
    Sleep 2s
    Type@45ms "Add dark mode to the settings page"
    Sleep 500ms
    Enter
    Sleep 3s
    Ctrl+P
    Sleep 1.2s
    Type "d"
    Sleep 2s
    Type@60ms "pmux ls"
    Sleep 400ms
    Enter
    Sleep 3s
    Type@40ms `cd ../api && pmux run -d -s loader-refactor claude "refactor the session loader to stream transcripts"`
    Sleep 600ms
    Enter
    Sleep 2s
    Type@60ms "pmux attach"
    Sleep 400ms
    Enter
    Sleep 2s
    Down
    Sleep 500ms
    Down
    Sleep 800ms
    Enter
    Sleep 2s
    Ctrl+B
    Ctrl+P
    Sleep 9s
    Type "/"
    Sleep 800ms
    Type@150ms "dark"
    Sleep 1.5s
    Tab
    Sleep 8s
"""


@scene
def demo(world, fonts):
    """A recording from an empty prompt: wrap and start agents, detach from
    them, attach back through the picker, then follow them all in the sidebar."""
    # A bash user: `pmux init` puts its wrapper hook in ~/.bashrc, which the
    # demo's shell reads, so `pmux wrap` takes effect at its next prompt.
    world.env["SHELL"] = "/bin/bash"
    world.cmd("pmux", "init")
    # System rc files set their own prompt (with the real user and host), so
    # set the demo's after them.
    rcfile = world.dir / "demo.bashrc"
    rcfile.write_text(f". ~/.bashrc\nPS1='{DEMO_PROMPT}'\n")
    world.outer("-f", str(world.outer_config), "new-session", "-d", "-s", "work",
                "-n", "dev", "-x", "160", "-y", "44", "-c", str(world.code / "api"),
                f"bash --noprofile --rcfile {rcfile}")
    world.outer("set-option", "-g", "@pmux_sidebar_open_focus", "sidebar")
    world.outer("set-option", "-w", "-t", "work:dev", "automatic-rename", "off")

    agents = {}  # (program, project) -> (pmux name, pane id, prompt)
    stop = threading.Event()

    def show(key, state):
        """Draw the typed agent `key`'s screen for `state`."""
        _, pane, prompt = agents[key]
        screen = DEMO_AGENTS[key]["screens"][state].replace("{prompt}", prompt)
        target = world.fake / "screens" / pane.lstrip("%")
        partial = target.with_suffix(".tmp")
        partial.write_text(screen)
        partial.rename(target)

    def watch_prompts():
        # An agent that has its first prompt starts working, and gets a
        # transcript, an identity, and an ai-attn record, as its hooks would
        # give it.
        reports = world.fake / "prompts"
        seen = set()
        while not stop.is_set():
            for report in sorted(reports.glob("*.json")) if reports.exists() else []:
                if report in seen:
                    continue
                seen.add(report)
                info = json.loads(report.read_text())
                key = (info["program"], Path(info["cwd"]).name)
                spec = DEMO_AGENTS.get(key)
                panes = world.pmux_tmux("list-panes", "-a", "-F", "#{pane_id}\t#{session_name}",
                                        check=False).stdout.splitlines()
                name = dict(line.split("\t", 1) for line in panes if "\t" in line).get(info["pane"])
                if not spec or not name:
                    print(f"warning: no demo agent for {info}", file=sys.stderr)
                    continue
                agents[key] = (name, info["pane"], info["prompt"])
                show(key, "working")
                stop.wait(LABEL_DELAY)
                world.processes.append((name, spec))
                world.mark_agent(name, spec, wait_for_start=True)
                world.cmd("pmux", "status-labels", check=False)
            stop.wait(0.1)

    def finish(key, state, reason):
        world.set_agent_state(agents[key][0], state, reason)
        show(key, state)

    codex, dark, loader = ("codex", "api"), ("claude", "web"), ("claude", "api")
    # Seconds after the sidebar opens.
    events = [
        (1.0, lambda: world.spawn_agent(DEMO_HELPER, DEMO_HELPER_SPEC, agents[loader][0])),
        (4.0, lambda: finish(dark, "waiting", "permission_prompt")),
        (6.5, lambda: finish(codex, "done", "agent-turn-complete")),
        (13.0, lambda: finish(loader, "done", "agent-turn-complete")),
        (15.0, lambda: world.set_agent_state(DEMO_HELPER, "done", "agent-turn-complete")),
    ]

    def timeline():
        # Wait for the sidebar: a second pane in the outer window.
        deadline = time.time() + 300
        while time.time() < deadline and not stop.is_set():
            panes = world.outer("list-panes", "-t", "work:dev", check=False).stdout.splitlines()
            if len(panes) >= 2:
                break
            time.sleep(0.05)
        else:
            print("warning: the sidebar never opened", file=sys.stderr)
            return
        start = time.time()
        for at, action in events:
            stop.wait(max(0.0, start + at - time.time()))
            if stop.is_set():
                return
            try:
                action()
            except KeyError as missing:
                print(f"warning: demo agent {missing} never started", file=sys.stderr)

    settle = 1.5
    env_prefix = " ".join(f"{k}={v}" for k, v in world.env.items() if k != "PATH")
    attach = f"env {env_prefix} PATH={world.env['PATH']} tmux -S {world.outer_socket} attach -t work"
    gif = OUTPUT_DIR / "demo.gif"
    mp4 = CACHE_DIR / "demo.mp4"
    tape = world.dir / "demo.tape"
    tape.write_text(textwrap.dedent(f"""\
        Output "{gif}"
        Output "{mp4}"
        Set Shell "bash"
        Set FontFamily "{FONT['family']}"
        Set FontSize {TERMINAL['font_size']}
        Set Width 1500
        Set Height 760
        Set Padding {TERMINAL['padding']}
        Set Framerate 24
        Set WindowBar Colorful
        Set Theme "{TERMINAL['theme']}"
        Hide
        Type@1ms "{attach}"
        Enter
        Sleep {settle}s
        Show
    """) + textwrap.dedent(DEMO_KEYS))
    threads = [threading.Thread(target=f, daemon=True) for f in (watch_prompts, timeline)]
    for thread in threads:
        thread.start()
    try:
        run(["vhs", str(tape)], env={**os.environ, "FONTCONFIG_FILE": str(fonts)})
    finally:
        stop.set()
    for thread in threads:
        thread.join(timeout=5)
    print(f"wrote {mp4} (upload it to a release; GitHub plays only uploaded video)")


def main():
    for name, config in doc_examples().items():
        SCENES[f"customization/{name}"] = example_scene(name, DOC_EXAMPLES[name], config)
    names = []
    for name in sys.argv[1:] or list(SCENES):
        if name == "customization":
            names += [scene for scene in SCENES if scene.startswith("customization/")]
        else:
            names.append(name)
    unknown = [n for n in names if n not in SCENES]
    if unknown:
        sys.exit(f"unknown scenes: {', '.join(unknown)} (have: {', '.join(SCENES)})")
    require_tools()
    run(["cargo", "build", "--release", "--quiet"], cwd=ROOT)
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    fonts = font_config()
    global OUTER_CONFIG_TEXT
    OUTER_CONFIG_TEXT = compile_outer_config()
    for name in names:
        with tempfile.TemporaryDirectory(prefix="pmux-screenshots-", ignore_cleanup_errors=True) as directory:
            world = World(directory)
            try:
                # The demo starts from an empty prompt and runs its own.
                world.start([] if name == "demo" else None)
                SCENES[name](world, fonts)
            finally:
                world.stop()
        if name != "demo":
            print(f"wrote docs/images/{name}.png")


if __name__ == "__main__":
    main()
