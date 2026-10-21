#!/usr/bin/env python3
"""Verify that /names displays and speaks only the current channel's cached users."""
import os
from pathlib import Path
import socketserver
import subprocess
import tempfile
import threading

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
                line = raw.decode().removesuffix("\r\n")
                command, _, arguments = line.partition(" ")
                command = command.upper()
                if command == "NICK":
                    nickname = arguments
                elif line == "CAP LS 302":
                    send("CAP * LS :multi-prefix userhost-in-names")
                elif command == "CAP" and arguments.startswith("REQ "):
                    send("CAP * ACK " + arguments.removeprefix("REQ "))
                elif line == "CAP END":
                    send(f":srv 001 {nickname} :Welcome", f":srv 422 {nickname} :No MOTD")
                elif command == "JOIN":
                    channel = arguments
                    members = (f"{nickname} @Bob" if channel == "#red"
                               else f"{nickname} @Mallory")
                    send(f":{nickname}!u@h JOIN {channel}",
                         f":srv 353 {nickname} = {channel} :{members}")
                    if channel == "#red":
                        send(f":srv 353 {nickname} = #red :@+alice!user@host")
                    send(f":srv 366 {nickname} {channel} :End of names")
                elif command == "TEST-MEMBERSHIP":
                    send(":Bob!u@h NICK Bobby", ":alice!u@h PART #red",
                         ":charlie!u@h JOIN #red")
                elif command == "PART":
                    send(f":{nickname}!u@h PART {arguments}")
                elif command == "NAMES":
                    self.server.names_requests.append(line)
                elif command == "PING":
                    send("PONG " + arguments)
                    if arguments.removeprefix(":") == "names-fence":
                        self.server.fence.set()
                elif command == "QUIT":
                    return
        except (BrokenPipeError, ConnectionResetError):
            return


def run():
    with Server(("127.0.0.1", 0), Session) as server, \
            tempfile.TemporaryDirectory(prefix="irctest-names-") as directory:
        server.names_requests = []
        server.fence = threading.Event()
        threading.Thread(target=server.serve_forever, daemon=True).start()
        Path(directory, "irctest.toml").write_text(f'''nickname = "me"
log = false
[[servers]]
name = "fake"
host = "127.0.0.1"
port = {server.server_address[1]}
auto_connect = true
''')
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

                def selected():
                    return [n.get_name() for n in nodes()
                            if n.get_role() == Atspi.Role.PAGE_TAB
                            and n.get_state_set().contains(Atspi.StateType.SELECTED)]

                def output():
                    for n in nodes():
                        if (n.get_role() == Atspi.Role.TEXT
                                and n.get_state_set().contains(Atspi.StateType.SHOWING)
                                and not n.get_state_set().contains(Atspi.StateType.EDITABLE)):
                            return Atspi.Text.get_text(n, 0, -1)
                    return ""

                def spoken():
                    log.seek(0)
                    return [line.removeprefix("SPEAK (interrupt): ")
                            for line in log.read().splitlines()
                            if line.startswith("SPEAK (interrupt): ")]

                def key(value):
                    subprocess.run(["xdotool", "key", "--clearmodifiers", value], check=True)

                def enter(value):
                    subprocess.run(["xdotool", "type", "--clearmodifiers", "--", value], check=True)
                    key("Return")

                def names(expected):
                    before = len(spoken())
                    enter("/names")
                    wait_for(lambda: len(spoken()) > before, "/names did not speak")
                    assert spoken()[before:] == [expected], spoken()[before:]
                    assert "Users on #" + expected.removeprefix("Users on ") in output(), output()

                wait_for(lambda: "Welcome" in output(), "not registered")
                window = subprocess.check_output(
                    ["xdotool", "search", "--onlyvisible", "--pid", str(client.pid),
                     "--name", "^irctest$"], text=True).splitlines()[0]
                subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)
                enter("/join #red")
                wait_for(lambda: selected() == ["#red"] and "alice" in output(), "red not ready")
                enter("/join #blue")
                wait_for(lambda: selected() == ["#blue"] and "Mallory" in output(), "blue not ready")
                names("Users on blue: Mallory, me")

                key("ctrl+shift+Tab")
                wait_for(lambda: selected() == ["#red"], "red not selected")
                names("Users on red: alice, Bob, me")
                for argument in ("#blue", "#red"):
                    before = len(spoken())
                    enter("/names " + argument)
                    wait_for(lambda: len(spoken()) > before, "argument not rejected")
                    assert spoken()[before].startswith("Usage: /names"), spoken()[before:]
                    assert selected() == ["#red"], "argument changed the current channel"

                enter("/raw TEST-MEMBERSHIP")
                wait_for(lambda: "charlie has joined #red" in output(), "membership updates missing")
                names("Users on red: Bobby, charlie, me")
                enter("/part")
                wait_for(lambda: "me has left #red" in output(), "channel not parted")
                before = len(spoken())
                enter("/names")
                wait_for(lambda: len(spoken()) > before, "parted channel not explained")
                assert "Join the channel" in spoken()[before], spoken()[before:]

                key("ctrl+shift+Tab")
                wait_for(lambda: selected() == ["fake"], "server not selected")
                before = len(spoken())
                enter("/names")
                wait_for(lambda: len(spoken()) > before, "server tab not explained")
                assert "current channel" in spoken()[before], spoken()[before:]
                enter("/query someone")
                wait_for(lambda: selected() == ["someone"], "query not selected")
                before = len(spoken())
                enter("/names")
                wait_for(lambda: len(spoken()) > before, "query tab not explained")
                assert "current channel" in spoken()[before], spoken()[before:]

                enter("/raw PING :names-fence")
                assert server.fence.wait(timeout=5), "server did not process commands"
                assert server.names_requests == [], server.names_requests
                print("PASS: /names current-channel display and speech, membership updates, argument rejection and no network NAMES")
            finally:
                client.terminate()
                client.wait(timeout=5)


if __name__ == "__main__":
    run()
