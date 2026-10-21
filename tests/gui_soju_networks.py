#!/usr/bin/env python3
"""Verify labels for queued soju children on an isolated virtual desktop."""
import os
from pathlib import Path
import socketserver
import subprocess
import tempfile
import threading

from gui_navigation import Atspi, BINARY, wait_for, walk


class Bouncer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


class Session(socketserver.StreamRequestHandler):
    def handle(self):
        network = None
        nickname = "reader"

        def send(*lines):
            self.wfile.write(("\r\n".join(lines) + "\r\n").encode())
            self.wfile.flush()

        try:
            for raw in self.rfile:
                line = raw.decode().strip()
                command, _, arguments = line.partition(" ")
                if command == "NICK":
                    nickname = arguments
                elif line == "CAP LS 302":
                    send("CAP * LS :sasl=PLAIN batch soju.im/bouncer-networks")
                elif line.startswith("CAP REQ "):
                    send("CAP * ACK :" + arguments.removeprefix("REQ ").removeprefix(":"))
                elif line == "AUTHENTICATE PLAIN":
                    send("AUTHENTICATE +")
                elif command == "AUTHENTICATE":
                    send(f":soju 903 {nickname} :Authenticated")
                elif line.startswith("BOUNCER BIND "):
                    network = arguments.split()[1]
                elif line == "CAP END":
                    if network is not None and network != "5":
                        self.server.registrations.wait(timeout=10)
                    support = "BOUNCER_NETID=" + network if network else "soju.im/SAFERATE"
                    send(f":soju 001 {nickname} :Welcome to soju",
                         f":soju 005 {nickname} {support} :supported",
                         f":soju 376 {nickname} :End MOTD")
                elif line == "BOUNCER LISTNETWORKS":
                    send("BATCH +networks soju.im/bouncer-networks",
                         *(f"@batch=networks BOUNCER NETWORK {i} name=Network{i}" for i in range(1, 6)),
                         "BATCH -networks")
                elif command == "PING":
                    send("PONG " + arguments)
                elif command == "QUIT":
                    return
        except (BrokenPipeError, ConnectionResetError, threading.BrokenBarrierError):
            return


def run():
    with Bouncer(("127.0.0.1", 0), Session) as bouncer, tempfile.TemporaryDirectory(prefix="irctest-soju-gui-") as directory:
        bouncer.registrations = threading.Barrier(4)
        threading.Thread(target=bouncer.serve_forever, daemon=True).start()
        Path(directory, "irctest.toml").write_text(f'''nickname = "reader"
log = false
[[servers]]
name = "account"
host = "127.0.0.1"
port = {bouncer.server_address[1]}
auto_connect = true
sasl_username = "reader"
sasl_password = "fixture-password"
''')
        with Path(directory, "client.log").open("w+") as log:
            client = subprocess.Popen([str(BINARY)], cwd=directory, stdout=log, stderr=log,
                                      env={**os.environ, "IRCTEST_SPEECH_LOG": "1", "GIO_USE_VFS": "local"})
            try:
                def tabs():
                    desktop = Atspi.get_desktop(0)
                    for index in range(desktop.get_child_count()):
                        app = desktop.get_child_at_index(index)
                        if app.get_process_id() == client.pid:
                            return [n.get_name() for n in walk(app) if n.get_role() == Atspi.Role.PAGE_TAB]
                    return []

                expected = ["account"] + [f"account / Network{i} [{i}]" for i in range(1, 6)]
                try:
                    wait_for(lambda: tabs() == expected,
                             "five network labels, including queued fifth child, must be correct")
                except AssertionError as error:
                    raise AssertionError(f"expected {expected!r}, got {tabs()!r}") from error
                print("PASS: all five discovered networks have labels, including the queued child")
            finally:
                client.terminate()
                client.wait(timeout=5)
                bouncer.shutdown()


if __name__ == "__main__":
    run()
