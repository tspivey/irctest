#!/usr/bin/env python3
"""Check native ignore controls, filtering, cancellation and persistence."""
import os
from pathlib import Path
import socketserver
import subprocess
import tempfile
import threading
import time
import tomllib

from gui_navigation import Atspi, BINARY, wait_for, walk


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


class Session(socketserver.StreamRequestHandler):
    def handle(self):
        nickname = "me"
        history_page = 0

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
                    send("CAP * ACK " + arguments.removeprefix("REQ "))
                elif line == "CAP END":
                    send(f":srv 001 {nickname} :Welcome", f":srv 422 {nickname} :No MOTD")
                    for channel in ("#red", "#blue"):
                        send(f":{nickname}!u@h JOIN {channel}",
                             f":srv 353 {nickname} = {channel} :{nickname} Bob Alice",
                             f":srv 366 {nickname} {channel} :End of names")
                elif command == "CHATHISTORY":
                    args = arguments.split()
                    if args[0] == "TARGETS":
                        send("BATCH +targets draft/chathistory-targets", "BATCH -targets")
                    else:
                        history_page += 1
                        channel = args[1]
                        batch = f"history{history_page}"
                        if channel.startswith("#"):
                            send(f"BATCH +{batch} chathistory {channel}",
                                 f"@batch={batch};time=2026-10-19T10:00:00Z;msgid=b{history_page} :bOB!u@h PRIVMSG {channel} :hidden saved Bob {channel}",
                                 f"@batch={batch};time=2026-10-19T10:01:00Z;msgid=a{history_page} :Alice!u@h PRIVMSG {channel} :saved Alice {channel}",
                                 f"BATCH -{batch}")
                        else:
                            send(f"BATCH +{batch} chathistory {channel}",
                                 f"@batch={batch};time=2026-10-19T10:00:00Z;msgid=b{history_page} :bOB!u@h PRIVMSG {nickname} :hidden saved Bob private",
                                 f"@batch={batch};time=2026-10-19T10:01:00Z;msgid=a{history_page} :{nickname}!u@h PRIVMSG {channel} :saved outgoing to Bob",
                                 f"BATCH -{batch}")
                elif command == "TEST-IGNORE":
                    phase = arguments
                    send(f":bOB!u@h PRIVMSG #blue :hidden Bob channel {phase}",
                         f":bOB!u@h PRIVMSG {nickname} :hidden Bob private {phase}",
                         f":bOB!u@h NOTICE {nickname} :hidden Bob notice {phase}",
                         f":bOB!u@h PRIVMSG {nickname} :\x01VERSION\x01",
                         f":bOB!u@h PRIVMSG #blue :\x01ACTION hidden Bob action {phase}\x01",
                         f":Alice!u@h PRIVMSG #red :hidden red channel {phase}",
                         f":Alice!u@h NOTICE #red :hidden red notice {phase}",
                         f":Alice!u@h PRIVMSG #blue :visible Alice {phase}")
                elif command == "TEST-UNIGNORE":
                    send(":Bob!u@h PRIVMSG #blue :unignored Bob",
                         ":Alice!u@h PRIVMSG #red :unignored red",
                         f":Bob!u@h PRIVMSG {nickname} :\x01VERSION\x01")
                elif command in ("PRIVMSG", "NOTICE"):
                    self.server.sent_messages.append(line)
                elif command == "TEST-SENT":
                    send(f":srv NOTICE {nickname} :send barrier {arguments}")
                elif command == "PING":
                    send("PONG " + arguments)
                elif command == "QUIT":
                    return
        except (BrokenPipeError, ConnectionResetError):
            return


def run():
    with Server(("127.0.0.1", 0), Session) as server, tempfile.TemporaryDirectory(prefix="irctest-ignore-") as directory:
        server.sent_messages = []
        threading.Thread(target=server.serve_forever, daemon=True).start()
        config_path = Path(directory, "irctest.toml")
        config_path.write_text(f'''nickname = "me"
log = false
[[servers]]
name = "test"
host = "127.0.0.1"
port = {server.server_address[1]}
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

                        def named(name):
                            return next((n for n in nodes() if n.get_name() == name), None)

                        def key(keys):
                            subprocess.run(["xdotool", "key", "--clearmodifiers", keys], check=True)
                            time.sleep(0.1)

                        def type_text(value):
                            subprocess.run(["xdotool", "type", "--clearmodifiers", "--", value], check=True)

                        def action(name):
                            node = wait_for(lambda: named(name), f"missing control {name}")
                            assert Atspi.Action.do_action(node, 0), f"could not activate {name}"
                            time.sleep(0.1)

                        def output():
                            return "\n".join(Atspi.Text.get_text(n, 0, -1) for n in nodes()
                                             if n.get_role() == Atspi.Role.TEXT
                                             and not n.get_state_set().contains(Atspi.StateType.EDITABLE))

                        def command(value):
                            input_node = next(n for n in nodes() if n.get_role() == Atspi.Role.TEXT
                                              and n.get_state_set().contains(Atspi.StateType.EDITABLE)
                                              and n.get_state_set().contains(Atspi.StateType.SHOWING))
                            assert Atspi.Component.grab_focus(input_node)
                            type_text(value)
                            key("Return")

                        def open_ignore():
                            key("alt+s")
                            key("i")
                            return wait_for(lambda: named("Ignore"), "Ignore dialog did not open")

                        def checkbox(channel):
                            return wait_for(lambda: named(f"Ignore this channel: {channel}"),
                                            f"missing checkbox for {channel}")

                        def checked(channel):
                            return checkbox(channel).get_state_set().contains(Atspi.StateType.CHECKED)

                        wait_for(lambda: named("#blue"), "channel tabs missing")
                        window = subprocess.check_output(["xdotool", "search", "--onlyvisible", "--pid",
                                                          str(client.pid), "--name", "^irctest$"], text=True).splitlines()[0]
                        subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)
                        open_ignore()
                        if attempt == 0:
                            assert not checked("#red") and not checked("#blue"), "channels ignored by default"
                            # The name field starts focused; Tab and Enter add a name.
                            type_text("BOB")
                            key("Tab")
                            key("Return")
                            key("Tab")
                            key("Tab")
                            assert checkbox("#blue").get_state_set().contains(Atspi.StateType.FOCUSED), "Tab did not reach channel checkboxes"
                            key("Tab")
                            assert checkbox("#red").get_state_set().contains(Atspi.StateType.FOCUSED)
                            key("space")
                            assert checked("#red"), "Space did not check channel"
                            action("OK")
                            saved = tomllib.loads(config_path.read_text())
                            assert sorted(saved["ignored"]) == ["#red", "BOB"], saved
                            # Cancellation discards both a removed channel and a typed name.
                            open_ignore()
                            type_text("Mallory")
                            action("Add")
                            assert Atspi.Component.grab_focus(checkbox("#red"))
                            key("space")
                            action("Cancel")
                            assert tomllib.loads(config_path.read_text())["ignored"] == saved["ignored"]
                        else:
                            assert checked("#red") and not checked("#blue"), "saved channel checks lost"
                            action("Cancel")
                            wait_for(lambda: "saved Alice #blue" in output(), "allowed history missing")
                            assert "hidden saved Bob" not in output(), "ignored nickname leaked through history"
                            assert "saved Alice #red" not in output(), "ignored channel leaked through history"
                            command("/query Bob")
                            wait_for(lambda: "saved outgoing to Bob" in output(), "ignore hid our outgoing query history")
                            assert "hidden saved Bob private" not in output(), "ignored private history leaked"

                        # Select #blue so allowed live chat also exercises the speech path.
                        for _ in range(3):
                            if any(n.get_role() == Atspi.Role.PAGE_TAB and n.get_name().startswith("#blue")
                                   and n.get_state_set().contains(Atspi.StateType.SELECTED) for n in nodes()):
                                break
                            key("ctrl+Tab")
                        assert any(n.get_role() == Atspi.Role.PAGE_TAB and n.get_name().startswith("#blue")
                                   and n.get_state_set().contains(Atspi.StateType.SELECTED) for n in nodes()), "could not select #blue"
                        command(f"/quote TEST-IGNORE phase{attempt}")
                        wait_for(lambda: f"visible Alice phase{attempt}" in output(), "allowed live message missing")
                        assert f"hidden Bob channel phase{attempt}" not in output()
                        assert f"hidden Bob private phase{attempt}" not in output()
                        assert f"hidden Bob notice phase{attempt}" not in output()
                        assert f"hidden Bob action phase{attempt}" not in output()
                        assert f"hidden red channel phase{attempt}" not in output()
                        assert f"hidden red notice phase{attempt}" not in output()
                        assert not any(n.get_role() == Atspi.Role.PAGE_TAB and n.get_name() == "bOB" for n in nodes()), "ignore opened a query tab"
                        log.seek(0)
                        spoken = log.read()
                        assert "hidden" not in spoken, "ignored message was spoken"

                        if attempt == 0:
                            for value in [
                                "/msg bOB blocked-outgoing-msg",
                                "/msg #RED blocked-outgoing-channel",
                                "/msg Alice,bOB blocked-outgoing-list",
                                "/msg @#RED blocked-outgoing-status",
                                "/notice bOB blocked-outgoing-notice",
                                "/ctcp bOB VERSION",
                                "/raw PRIVMSG bOB :blocked-outgoing-raw",
                                "/raw PRIVMSG bOB blocked-outgoing-raw extra",
                                "/quote NOTICE #rED :blocked-outgoing-raw-notice",
                                "/quote notice #rED blocked-outgoing-raw-notice extra",
                                "/PRIVMSG bOB :blocked-outgoing-command",
                                "/query bOB blocked-outgoing-query",
                                "blocked-outgoing-current",
                                "/me blocked-outgoing-action",
                            ]:
                                before = output().count("Person or channel on ignore.")
                                command(value)
                                wait_for(lambda: output().count("Person or channel on ignore.") > before,
                                         f"missing ignore explanation for {value!r}")
                            # Plain messages in an ignored channel use the same rule.
                            for _ in range(4):
                                if any(n.get_role() == Atspi.Role.PAGE_TAB and n.get_name().startswith("#red")
                                       and n.get_state_set().contains(Atspi.StateType.SELECTED) for n in nodes()):
                                    break
                                key("ctrl+Tab")
                            before = output().count("Person or channel on ignore.")
                            command("blocked-outgoing-channel-current")
                            wait_for(lambda: output().count("Person or channel on ignore.") > before,
                                     "plain ignored-channel send was not explained")
                            command("/quote TEST-SENT blocked")
                            wait_for(lambda: "send barrier blocked" in output(), "outgoing barrier missing")
                            assert server.sent_messages == [], f"ignored text reached server: {server.sent_messages}"
                            assert "blocked-outgoing" not in output(), "blocked message was locally echoed"
                            log.seek(0)
                            assert "Person or channel on ignore." in log.read(), "ignore explanation was silent"

                        if attempt == 1:
                            open_ignore()
                            # Uncheck a channel and remove the remaining nickname entry.
                            assert Atspi.Component.grab_focus(checkbox("#red"))
                            key("space")
                            key("Tab")
                            key("Home")
                            key("Tab")
                            assert named("Remove").get_state_set().contains(Atspi.StateType.FOCUSED), "Tab did not reach Remove from the ignore list"
                            key("Return")
                            action("OK")
                            assert tomllib.loads(config_path.read_text())["ignored"] == []
                            command("/quote TEST-UNIGNORE")
                            wait_for(lambda: "unignored Bob" in output() and "unignored red" in output(),
                                     "removing ignores did not restore messages")
                            command("/msg Bob allowed-outgoing-person")
                            command("/msg #red allowed-outgoing-channel")
                            command("/quote TEST-SENT allowed")
                            wait_for(lambda: "send barrier allowed" in output(), "allowed-send barrier missing")
                            assert len(server.sent_messages) == 3, server.sent_messages
                            assert any("allowed-outgoing-person" in line for line in server.sent_messages)
                            assert any("allowed-outgoing-channel" in line for line in server.sent_messages)
                            assert any(line.startswith("NOTICE Bob ") and "VERSION" in line for line in server.sent_messages), "automatic replies stayed blocked after removal"
                        action("Exit")
                        client.wait(timeout=5)
                    finally:
                        if client.poll() is None:
                            client.terminate()
                            client.wait(timeout=5)
            print("PASS: ignore controls, live/history filtering, blocked outgoing commands, automatic replies, silence, cancel, restart and removal")
        finally:
            server.shutdown()


if __name__ == "__main__":
    run()
