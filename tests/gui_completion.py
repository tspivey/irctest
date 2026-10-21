#!/usr/bin/env python3
"""Check Tab nick completion and F6 focus switching against a local fake server."""
import os
from pathlib import Path
import socketserver
import subprocess
import tempfile
import threading
import time

from gui_navigation import Atspi, BINARY, wait_for, walk


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


class Session(socketserver.StreamRequestHandler):
    def handle(self):
        nickname = "me"

        def send(*lines):
            self.wfile.write(("\r\n".join(lines) + "\r\n").encode())
            self.wfile.flush()

        try:
            for raw in self.rfile:
                line = raw.decode().strip()
                command, _, arguments = line.partition(" ")
                if command == "NICK":
                    nickname = arguments
                elif command == "USER":
                    send(f":srv 001 {nickname} :Welcome", f":srv 422 {nickname} :No MOTD")
                elif command == "JOIN":
                    send(f":{nickname}!u@h JOIN #chan",
                         f":srv 353 {nickname} = #chan :{nickname} @Bob barry alice",
                         f":srv 366 {nickname} #chan :End of names",
                         ":barry!u@h PRIVMSG #chan :hello")
                elif command == "PING":
                    send("PONG " + arguments)
                elif command == "QUIT":
                    return
        except (BrokenPipeError, ConnectionResetError):
            return


def run():
    with Server(("127.0.0.1", 0), Session) as server, \
            tempfile.TemporaryDirectory(prefix="irctest-completion-") as directory:
        threading.Thread(target=server.serve_forever, daemon=True).start()
        Path(directory, "irctest.toml").write_text(f'''nickname = "me"
log = false
[[servers]]
name = "fake"
host = "127.0.0.1"
port = {server.server_address[1]}
auto_connect = true
''')
        log_path = Path(directory, "client.log")
        with log_path.open("w+") as log:
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

                def focused(editable):
                    for n in nodes():
                        states = n.get_state_set()
                        if (n.get_role() == Atspi.Role.TEXT
                                and states.contains(Atspi.StateType.FOCUSED)
                                and states.contains(Atspi.StateType.EDITABLE) == editable):
                            return n
                    return None

                def key(keys):
                    subprocess.run(["xdotool", "key", "--clearmodifiers", keys], check=True)
                    time.sleep(0.15)

                def type_text(value):
                    subprocess.run(["xdotool", "type", "--clearmodifiers", "--", value], check=True)
                    time.sleep(0.1)

                def input_text():
                    node = focused(True)
                    return node and Atspi.Text.get_text(node, 0, -1)

                def expect(value):
                    wait_for(lambda: input_text() == value,
                             f"expected input {value!r}, got {input_text()!r}")

                def spoken():
                    log.seek(0)
                    return [line.removeprefix("SPEAK (interrupt): ")
                            for line in log.read().splitlines()
                            if line.startswith("SPEAK (interrupt): ")]

                def clear():
                    key("ctrl+a")
                    key("BackSpace")
                    expect("")

                wait_for(lambda: selected() == ["fake"], "server tab did not appear")
                window = subprocess.check_output(
                    ["xdotool", "search", "--onlyvisible", "--pid", str(client.pid),
                     "--name", "^irctest$"], text=True).splitlines()[0]
                subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)
                wait_for(lambda: focused(True), "Message input not focused")
                wait_for(lambda: any("Welcome" in Atspi.Text.get_text(n, 0, -1) for n in nodes()
                                     if n.get_role() == Atspi.Role.TEXT), "not registered")
                type_text("/join #chan")
                key("Return")
                wait_for(lambda: selected() == ["#chan"], "channel tab not selected")
                wait_for(lambda: any("hello" in Atspi.Text.get_text(n, 0, -1) for n in nodes()
                                     if n.get_role() == Atspi.Role.TEXT), "message not shown")

                # The most recent speaker comes first, then the rest alphabetically.
                type_text("b")
                key("Tab")
                expect("barry: ")
                key("Tab")
                expect("Bob: ")
                key("Tab")
                expect("barry: ")
                assert spoken()[-3:] == ["barry", "Bob", "barry"], spoken()
                clear()

                type_text("hi A")
                key("Tab")
                expect("hi alice ")
                # Text after the cursor is kept.
                clear()
                type_text("x B y")
                key("Left")
                key("Left")
                key("Tab")
                expect("x barry  y")

                # No match, and our own nick isn't offered: the input is unchanged.
                clear()
                before = len(spoken())
                type_text("zz")
                key("Tab")
                type_text(" m")
                key("Tab")
                expect("zz m")
                assert len(spoken()) == before, spoken()

                # F6 moves between Message and Output; Shift+Tab still leaves Message.
                key("F6")
                wait_for(lambda: focused(False), "F6 did not reach Output")
                key("F6")
                wait_for(lambda: focused(True), "F6 did not return to Message")
                key("shift+Tab")
                wait_for(lambda: focused(False), "Shift+Tab did not reach Output")
                print("PASS: nick completion order, cycling, suffixes, no-match, and F6 focus")
            finally:
                client.terminate()
                client.wait(timeout=5)


if __name__ == "__main__":
    run()
