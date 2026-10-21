#!/usr/bin/env python3
"""Check channel context across restarts using a local fake soju server."""
import os
from datetime import datetime, timedelta, timezone
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
        nickname = "reader"
        registration = 0

        def message(index, batch=True):
            timestamp = (datetime(2026, 10, 20, 10, tzinfo=timezone.utc)
                         + timedelta(minutes=index)).isoformat(timespec="milliseconds").replace("+00:00", "Z")
            tags = ("batch=history;" if batch else "") + "time=" + timestamp
            if self.server.message_tags:
                tags += f";msgid=message{index}"
            body = "live after gap" if index == 1204 else f"saved context {index}"
            return f"@{tags} :other!user@host PRIVMSG #history :{body}"

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
                    send("CAP * LS :batch server-time message-tags draft/chathistory")
                elif line.startswith("CAP REQ "):
                    send("CAP * ACK :" + arguments.removeprefix("REQ ").removeprefix(":"))
                elif line == "CAP END":
                    self.server.registrations += 1
                    registration = self.server.registrations
                    if not self.server.message_tags:
                        send("CAP * DEL :message-tags")
                    send(f":soju 001 {nickname} :Welcome to soju",
                         f":soju 005 {nickname} BOUNCER_NETID=42 CHATHISTORY=1000 MSGREFTYPES=timestamp :supported",
                         f":soju 422 {nickname} :No MOTD",
                         f":{nickname}!user@host JOIN #history",
                         f":soju 366 {nickname} #history :End of names")
                elif command == "CHATHISTORY":
                    args = arguments.split()
                    self.server.requests.append(args)
                    if args[0] == "TARGETS":
                        send("BATCH +targets draft/chathistory-targets", "BATCH -targets")
                    else:
                        if self.server.capped and registration == 2:
                            if args[0] == "LATEST":
                                self.server.unexpected_context.set()
                                messages = range(1105, 1205)
                            else:
                                messages = range(self.server.next_message, self.server.next_message + 100)
                                self.server.next_message += 100
                        else:
                            messages = [1, 2, 3] if args[0] == "LATEST" else [3]
                        send("BATCH +history chathistory #history",
                             *(message(i) for i in messages))
                        if self.server.capped and registration == 2 and self.server.next_message == 1004 and args[0] == "AFTER":
                            send(message(1204, batch=False))
                        send("BATCH -history")
                elif command == "PING":
                    send("PONG " + arguments)
                elif command == "QUIT":
                    return
        except (BrokenPipeError, ConnectionResetError):
            return


def run_case(message_tags, capped=False):
    with Bouncer(("127.0.0.1", 0), Session) as bouncer, tempfile.TemporaryDirectory(prefix="irctest-history-gui-") as directory:
        bouncer.requests = []
        bouncer.message_tags = message_tags
        bouncer.capped = capped
        bouncer.registrations = 0
        bouncer.next_message = 4
        bouncer.unexpected_context = threading.Event()
        threading.Thread(target=bouncer.serve_forever, daemon=True).start()
        Path(directory, "irctest.toml").write_text(f'''nickname = "reader"
log = false
[[servers]]
name = "account"
host = "127.0.0.1"
port = {bouncer.server_address[1]}
auto_connect = true
''')
        try:
            for attempt in range(2):
                with Path(directory, f"client-{attempt}.log").open("w+") as log:
                    client = subprocess.Popen([str(BINARY)], cwd=directory, stdout=log, stderr=log,
                                              env={**os.environ, "IRCTEST_SPEECH_LOG": "1", "GIO_USE_VFS": "local"})
                    try:
                        def nodes():
                            desktop = Atspi.get_desktop(0)
                            for index in range(desktop.get_child_count()):
                                app = desktop.get_child_at_index(index)
                                if app.get_process_id() == client.pid:
                                    yield from walk(app)

                        def restored():
                            for node in nodes():
                                if (node.get_role() == Atspi.Role.TEXT
                                        and not node.get_state_set().contains(Atspi.StateType.EDITABLE)):
                                    text = Atspi.Text.get_text(node, 0, -1)
                                    if capped and attempt == 1:
                                        if "Catch-up reached 1000 messages" not in text or "saved context 1003" not in text:
                                            continue
                                        assert sum("saved context " in line for line in text.splitlines()) == 1000
                                        assert text.index("saved context 4") < text.index("saved context 1003") < text.index("live after gap"), "capped catch-up reordered live traffic"
                                        return True
                                    if all(sum(line.endswith(f"saved context {i}") for line in text.splitlines()) == 1 for i in (1, 2, 3)):
                                        assert text.index("saved context 1") < text.index("saved context 2") < text.index("saved context 3"), "restored history is out of order"
                                        return True
                            return False

                        try:
                            wait_for(restored, f"channel context missing on {'restart' if attempt else 'first login'}")
                        except AssertionError as error:
                            raise AssertionError(f"{error}; requests={bouncer.requests!r}") from error
                        if capped and attempt == 1:
                            assert not bouncer.unexpected_context.wait(timeout=0.5), "capped catch-up must keep its ordered context, not start a new LATEST across the remaining gap"
                        # Use the real shutdown path so synchronization state is flushed.
                        exit_action = next(node for node in nodes() if node.get_name() == "Exit")
                        assert Atspi.Action.do_action(exit_action, 0)
                        client.wait(timeout=5)
                        log.seek(0)
                        assert "saved context" not in log.read(), "history was spoken"
                    finally:
                        if client.poll() is None:
                            client.terminate()
                            client.wait(timeout=5)
            print(f"PASS: channel history restores in order without duplicates on login and restart; message-tags={message_tags}, capped={capped}")
        finally:
            bouncer.shutdown()


if __name__ == "__main__":
    for message_tags in (True, False):
        run_case(message_tags)
    run_case(True, capped=True)
