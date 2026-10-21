#!/usr/bin/env python3
"""Check initial Output position and reading anchors in native GTK controls."""
import os
from pathlib import Path
import subprocess
import tempfile
import threading

from gui_history import Bouncer, Session
from gui_navigation import Atspi, BINARY, wait_for, walk


def run_case(config, history=False):
    with tempfile.TemporaryDirectory(prefix="irctest-output-cursor-") as directory:
        Path(directory, "irctest.toml").write_text(config)
        with Path(directory, "client.log").open("w+") as log:
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

                def output_containing(value):
                    return next((node for node in nodes()
                                 if node.get_role() == Atspi.Role.TEXT
                                 and not node.get_state_set().contains(Atspi.StateType.EDITABLE)
                                 and value in Atspi.Text.get_text(node, 0, -1)), None)

                def at_end(output):
                    return (Atspi.Text.get_caret_offset(output)
                            == Atspi.Text.get_character_count(output))

                marker = "saved context 3" if history else "Not connected."
                output = wait_for(lambda: output_containing(marker), "startup output missing")
                assert at_end(output), (
                    f"startup Output cursor is {Atspi.Text.get_caret_offset(output)}, "
                    f"expected bottom {Atspi.Text.get_character_count(output)}"
                )
                if not history:
                    print("PASS: disconnected startup Output cursor starts at bottom")
                    return

                server_output = wait_for(lambda: output_containing("Connected."),
                                         "server startup output missing")
                assert at_end(server_output), "server startup Output cursor is not at bottom"

                window = subprocess.check_output(
                    ["xdotool", "search", "--onlyvisible", "--pid", str(client.pid),
                     "--name", "^irctest$"], text=True).splitlines()[0]
                subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)

                def key(value):
                    subprocess.run(["xdotool", "key", "--clearmodifiers", value], check=True)

                def focus(control):
                    assert Atspi.Component.grab_focus(control)
                    wait_for(lambda: control.get_state_set().contains(Atspi.StateType.FOCUSED),
                             "control did not receive focus")

                def append(value):
                    input_control = next(node for node in nodes()
                                         if node.get_role() == Atspi.Role.TEXT
                                         and node.get_state_set().contains(Atspi.StateType.SHOWING)
                                         and node.get_state_set().contains(Atspi.StateType.EDITABLE))
                    focus(input_control)
                    subprocess.run(["xdotool", "type", "--clearmodifiers", "--", value], check=True)
                    key("Return")
                    wait_for(lambda: value in Atspi.Text.get_text(output, 0, -1),
                             "new output missing")

                key("ctrl+Tab")
                wait_for(lambda: any(node.get_role() == Atspi.Role.PAGE_TAB
                                     and node.get_name().startswith("#history")
                                     and node.get_state_set().contains(Atspi.StateType.SELECTED)
                                     for node in nodes()), "history tab was not selected")
                focus(output)
                assert at_end(output), "first Output focus reset cursor to top"
                key("ctrl+Home")
                wait_for(lambda: Atspi.Text.get_caret_offset(output) == 0,
                         "could not move reading cursor to top")
                append("cursor preservation")
                assert Atspi.Text.get_caret_offset(output) == 0, "new message moved reading cursor"
                focus(output)
                assert Atspi.Text.get_caret_offset(output) == 0, "returning to Output lost cursor"
                key("shift+Right")
                wait_for(lambda: Atspi.Text.get_n_selections(output) == 1, "selection missing")
                before = Atspi.Text.get_selection(output, 0)
                append("selection preservation")
                after = Atspi.Text.get_selection(output, 0)
                assert (after.start_offset, after.end_offset) == (before.start_offset, before.end_offset), \
                    "new message moved selection"
                print("PASS: restored startup history starts at bottom; later messages and refocus preserve reading cursor and selection")
            finally:
                client.terminate()
                client.wait(timeout=5)


if __name__ == "__main__":
    # Check history first so the original multi-line startup regression fails clearly.
    with Bouncer(("127.0.0.1", 0), Session) as bouncer:
        bouncer.requests = []
        bouncer.message_tags = True
        bouncer.capped = False
        bouncer.registrations = 0
        threading.Thread(target=bouncer.serve_forever, daemon=True).start()
        try:
            run_case(f'''nickname = "reader"
log = false
[[servers]]
name = "account"
host = "127.0.0.1"
port = {bouncer.server_address[1]}
auto_connect = true
''', history=True)
        finally:
            bouncer.shutdown()
    run_case('nickname = "reader"\nlog = false\n')
