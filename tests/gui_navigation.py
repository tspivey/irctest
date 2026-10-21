#!/usr/bin/env python3
"""Native GTK regression check. Run under dbus-run-session and xvfb-run.

Requires Python GObject/AT-SPI, xdotool, Xvfb and a built debug client.
Uses temporary configuration, unreachable loopback endpoints, and logged speech.
"""
import os
from pathlib import Path
import subprocess
import tempfile
import time

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi

BINARY = Path(__file__).resolve().parents[1] / "target/debug/irctest"


def walk(node):
    yield node
    for index in range(node.get_child_count()):
        yield from walk(node.get_child_at_index(index))


def wait_for(check, description):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError(description)


def run():
    with tempfile.TemporaryDirectory(prefix="irctest-navigation-") as directory:
        Path(directory, "irctest.toml").write_text('''nickname = "navtest"
log = false
[[servers]]
name = "account / Network One"
host = "127.0.0.1"
port = 1
auto_connect = true
[[servers]]
name = "account / Network Two"
host = "127.0.0.1"
port = 1
auto_connect = true
''')
        with open(Path(directory, "client.log"), "w+") as log:
            client = subprocess.Popen(
                [str(BINARY)], cwd=directory, stdout=log, stderr=log,
                env={**os.environ, "IRCTEST_SPEECH_LOG": "1", "GIO_USE_VFS": "local"},
            )
            try:
                def nodes():
                    desktop = Atspi.get_desktop(0)
                    for index in range(desktop.get_child_count()):
                        app = desktop.get_child_at_index(index)
                        if app.get_process_id() == client.pid:
                            yield from walk(app)

                def selected():
                    return [n.get_name() for n in nodes()
                            if n.get_role() == Atspi.Role.PAGE_TAB
                            and n.get_state_set().contains(Atspi.StateType.SELECTED)]

                def input_focused():
                    return any(n.get_role() == Atspi.Role.TEXT
                               and n.get_state_set().contains(Atspi.StateType.FOCUSED)
                               and n.get_state_set().contains(Atspi.StateType.EDITABLE)
                               for n in nodes())

                def expect(name):
                    wait_for(lambda: selected() == [name],
                             f"expected selected tab {name!r}, got {selected()!r}")
                    wait_for(input_focused, f"Message input not focused in {name}")

                def key(keys):
                    subprocess.run(["xdotool", "key", "--clearmodifiers", keys], check=True)
                    time.sleep(0.1)

                def text(value):
                    subprocess.run(["xdotool", "type", "--clearmodifiers", "--", value], check=True)

                wait_for(lambda: len(selected()) == 1, "client notebook did not appear")
                window = subprocess.check_output(
                    ["xdotool", "search", "--onlyvisible", "--pid", str(client.pid),
                     "--name", "^irctest$"], text=True).splitlines()[0]
                subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)
                expect("account / Network One")
                key("ctrl+Tab")
                expect("account / Network Two")
                key("ctrl+Tab")
                expect("account / Network One")
                key("ctrl+shift+Tab")
                expect("account / Network Two")
                key("ctrl+shift+Tab")
                expect("account / Network One")
                for nick in ["alice", "bob"]:
                    text(f"/query {nick}")
                    key("Return")
                    expect(nick)
                text("unsent draft")
                key("ctrl+Tab")
                expect("account / Network Two")
                key("ctrl+shift+Tab")
                expect("bob")
                assert any(n.get_state_set().contains(Atspi.StateType.FOCUSED)
                           and Atspi.Text.get_text(n, 0, -1) == "unsent draft"
                           for n in nodes() if n.get_role() == Atspi.Role.TEXT), "draft lost"
                key("ctrl+Page_Down")
                expect("account / Network Two")
                key("ctrl+Page_Down")
                expect("account / Network One")
                key("ctrl+Page_Up")
                expect("account / Network Two")
                key("ctrl+Page_Up")
                expect("account / Network One")
                # F6 reaches Output (Tab completes nicks); navigation must work there too.
                key("F6")
                wait_for(lambda: any(n.get_role() == Atspi.Role.TEXT
                                     and n.get_state_set().contains(Atspi.StateType.FOCUSED)
                                     and not n.get_state_set().contains(Atspi.StateType.EDITABLE)
                                     for n in nodes()), "F6 did not reach Output")
                key("ctrl+Tab")
                expect("alice")
                # Menu navigation uses the same actions as the keyboard shortcuts.
                key("alt+v")
                key("n")
                expect("bob")
                key("alt+v")
                key("p")
                expect("alice")
                print("PASS: forward/backward tabs, network wrap, Message/Output focus, drafts and menus")
            finally:
                client.terminate()
                client.wait(timeout=5)


if __name__ == "__main__":
    run()
