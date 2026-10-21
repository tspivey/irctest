#!/usr/bin/env python3
"""Exercise /ctcp through native Message input, with and without server echo."""
import os
from pathlib import Path
import queue
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
        nickname = "reader"

        def send(*lines):
            self.wfile.write(("\r\n".join(lines) + "\r\n").encode())
            self.wfile.flush()

        try:
            for raw in self.rfile:
                line = raw.decode().removesuffix("\r\n")
                command, _, arguments = line.partition(" ")
                if command == "NICK":
                    nickname = arguments
                elif line == "CAP LS 302":
                    send("CAP * LS :" + ("echo-message" if self.server.echo else ""))
                elif line.startswith("CAP REQ "):
                    send("CAP * ACK :echo-message")
                elif line == "CAP END":
                    send(f":srv 001 {nickname} :Welcome", f":srv 422 {nickname} :No MOTD")
                elif command == "PRIVMSG":
                    self.server.requests.put(line)
                    if self.server.echo:
                        send(f":{nickname}!u@h {line}")
                    _, _, body = arguments.partition(" ")
                    body = body.removeprefix(":")
                    if body == "\x01VERSION\x01":
                        body = "\x01VERSION TestClient 1.0\x01"
                    send(f":bob!u@h NOTICE {nickname} :{body}")
                elif command == "PING":
                    send("PONG " + arguments)
                elif command == "QUIT":
                    return
        except (BrokenPipeError, ConnectionResetError):
            return


def run_case(echo):
    with Server(("127.0.0.1", 0), Session) as server, \
            tempfile.TemporaryDirectory(prefix="irctest-ctcp-") as directory:
        server.echo = echo
        server.requests = queue.Queue()
        threading.Thread(target=server.serve_forever, daemon=True).start()
        Path(directory, "irctest.toml").write_text(f'''nickname = "reader"
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

                def output():
                    return "\n".join(Atspi.Text.get_text(n, 0, -1) for n in nodes()
                                     if n.get_role() == Atspi.Role.TEXT
                                     and not n.get_state_set().contains(Atspi.StateType.EDITABLE))

                def enter(text):
                    subprocess.run(["xdotool", "type", "--clearmodifiers", "--", text], check=True)
                    subprocess.run(["xdotool", "key", "--clearmodifiers", "Return"], check=True)

                wait_for(lambda: "Welcome" in output(), "not registered")
                window = subprocess.check_output(
                    ["xdotool", "search", "--onlyvisible", "--pid", str(client.pid),
                     "--name", "^irctest$"], text=True).splitlines()[0]
                subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)
                wait_for(lambda: any(n.get_role() == Atspi.Role.TEXT
                                     and n.get_state_set().contains(Atspi.StateType.FOCUSED)
                                     and n.get_state_set().contains(Atspi.StateType.EDITABLE)
                                     for n in nodes()), "Message input not focused")

                enter("/ctcp bob version")
                assert server.requests.get(timeout=5) == "PRIVMSG bob \x01VERSION\x01"
                wait_for(lambda: "CTCP VERSION reply from bob: TestClient 1.0" in output(),
                         "VERSION reply missing")
                assert output().count("CTCP VERSION from reader") == 1, output()

                enter("/ctcp bob")
                wait_for(lambda: "Usage: /ctcp" in output(), "missing argument not explained")
                enter("/ctcp bob ping 123  456")
                assert server.requests.get(timeout=5) == "PRIVMSG bob :\x01PING 123  456\x01"
                wait_for(lambda: "CTCP PING reply from bob: 123  456" in output(),
                         "PING reply missing or parameters changed")
                assert output().count("CTCP PING from reader 123  456") == 1, output()
                assert not any(n.get_role() == Atspi.Role.PAGE_TAB and n.get_name() == "bob"
                               for n in nodes()), "CTCP opened an unwanted private conversation"
                log.flush()
                log.seek(0)
                speech = [line for line in log.read().splitlines() if line.startswith("SPEAK")]
                assert not any("CTCP" in line for line in speech), speech
                assert any("Usage: /ctcp" in line for line in speech), "usage help was silent"

                enter("/query bob")
                wait_for(lambda: any(n.get_role() == Atspi.Role.PAGE_TAB
                                     and n.get_name() == "bob"
                                     and n.get_state_set().contains(Atspi.StateType.SELECTED)
                                     for n in nodes()), "private conversation did not open")
                enter("/me waves")
                assert server.requests.get(timeout=5) == "PRIVMSG bob :\x01ACTION waves\x01"

                def action_spoken():
                    log.seek(0)
                    return "SPEAK: reader waves" in log.read()

                wait_for(action_spoken, "ACTION speech was lost")
                print(f"PASS: /ctcp framing, replies, validation, quiet single display and ACTION speech; echo={echo}")
            finally:
                client.terminate()
                client.wait(timeout=5)


if __name__ == "__main__":
    run_case(False)
    run_case(True)
