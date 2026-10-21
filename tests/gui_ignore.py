#!/usr/bin/env python3
"""Check native ignore controls, filtering, cancellation and persistence."""
import os
from pathlib import Path
import socketserver
import subprocess
import sys
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
                    send("CAP * LS :" if self.server.other else "CAP * LS :batch server-time message-tags draft/chathistory")
                elif line.startswith("CAP REQ "):
                    send("CAP * ACK " + arguments.removeprefix("REQ "))
                elif line == "CAP END":
                    send(f":srv 001 {nickname} :Welcome", f":srv 422 {nickname} :No MOTD")
                    for channel in (("#other",) if self.server.other else ("#red", "#blue")):
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
                elif command == "TEST-OTHER":
                    send(":Bob!u@h PRIVMSG #other :visible Bob other server",
                         ":Alice!u@h PRIVMSG #red :visible red other server",
                         f":Bob!u@h PRIVMSG {nickname} :\x01VERSION\x01")
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
    with Server(("127.0.0.1", 0), Session) as server, Server(("127.0.0.1", 0), Session) as other, tempfile.TemporaryDirectory(prefix="irctest-ignore-") as directory:
        server.sent_messages = []
        server.other = False
        other.sent_messages = []
        other.other = True
        threading.Thread(target=other.serve_forever, daemon=True).start()
        threading.Thread(target=server.serve_forever, daemon=True).start()
        config_path = Path(directory, "irctest.toml")
        config_path.write_text(f'''nickname = "me"
log = false
[[servers]]
name = "test"
host = "127.0.0.1"
port = {server.server_address[1]}
auto_connect = true
[[servers]]
name = "other"
host = "127.0.0.1"
port = {other.server_address[1]}
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
                            if node.get_state_set().contains(Atspi.StateType.FOCUSABLE):
                                assert Atspi.Component.grab_focus(node), f"could not focus {name}"
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
                            dialog = wait_for(lambda: named("Ignore"), "Ignore dialog did not open")
                            window = subprocess.check_output(["xdotool", "search", "--all", "--onlyvisible", "--pid",
                                                              str(client.pid), "--name", "^Ignore$"], text=True).splitlines()[0]
                            subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)
                            wait_for(lambda: any(n.get_state_set().contains(Atspi.StateType.FOCUSED)
                                                 for n in walk(server_combo())),
                                     "server selector did not receive initial focus")
                            focus_server()
                            return dialog

                        def checkbox(name):
                            return wait_for(lambda: next((n for n in nodes()
                                if n.get_role() == Atspi.Role.CHECK_BOX and n.get_name() == name), None),
                                f"missing ignored-name checkbox for {name}")

                        def checked(name):
                            return checkbox(name).get_state_set().contains(Atspi.StateType.CHECKED)

                        def server_combo():
                            return wait_for(lambda: next((n for n in nodes()
                                if n.get_role() == Atspi.Role.COMBO_BOX), None),
                                "missing server combo box")

                        def focus_server():
                            # GTK's read-only combo exposes focus on an inner text control.
                            # Enter it through keyboard navigation from the following field.
                            assert Atspi.Component.grab_focus(name_field())
                            key("shift+Tab")

                        def name_field():
                            return next(n for n in walk(named("Ignore")) if n.get_role() == Atspi.Role.TEXT
                                and n.get_state_set().contains(Atspi.StateType.EDITABLE)
                                and n.get_state_set().contains(Atspi.StateType.SHOWING))

                        def add_name(value):
                            assert Atspi.Component.grab_focus(name_field())
                            type_text(value)
                            action("Add")

                        def saved_ignores():
                            saved = tomllib.loads(config_path.read_text())
                            assert "ignored" not in saved, "ignores are still global"
                            return saved["servers"][0]["ignored"]

                        wait_for(lambda: named("#blue"), "channel tabs missing")
                        wait_for(lambda: named("#other"), "other server channel missing")
                        window = subprocess.check_output(["xdotool", "search", "--all", "--onlyvisible", "--pid",
                                                          str(client.pid), "--name", "^irctest$"], text=True).splitlines()[0]
                        subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)
                        if attempt == 0:
                            # Renaming a connected server must preserve its ignore association.
                            key("ctrl+shift+s")
                            wait_for(lambda: named("Manage servers"), "server manager did not open")
                            assert Atspi.Component.grab_focus(named("Edit..."))
                            key("space")
                            editor = wait_for(lambda: named("Edit server"), "server editor did not open")
                            server_name = next(n for n in walk(editor) if n.get_role() == Atspi.Role.TEXT
                                               and n.get_state_set().contains(Atspi.StateType.EDITABLE))
                            assert Atspi.Component.grab_focus(server_name)
                            key("ctrl+a")
                            type_text("renamed")
                            action("OK")
                            action("Close")
                        open_ignore()
                        if attempt == 0:
                            assert not any(n.get_role() == Atspi.Role.CHECK_BOX for n in nodes()), "channel checkbox section remains"
                            key("Tab")
                            assert name_field().get_state_set().contains(Atspi.StateType.FOCUSED), "name field must follow server selector"
                            type_text("BOB")
                            key("Tab")
                            key("Return")
                            add_name("#red")
                            # Switching servers replaces the list without saving yet.
                            focus_server()
                            key("End")
                            assert not any(n.get_role() == Atspi.Role.CHECK_BOX for n in nodes()), "ignore list leaked into another server"
                            add_name("OtherOnly")
                            assert not checked("OtherOnly")
                            focus_server()
                            key("Home")
                            assert not named("OtherOnly"), "another server's ignore leaked into this list"
                            assert not checked("BOB") and not checked("#red"), "removal checks must start unchecked"
                            assert not named("Remove ignore").get_state_set().contains(Atspi.StateType.ENABLED)
                            action("OK")
                            assert sorted(saved_ignores()) == ["#red", "BOB"]
                            assert tomllib.loads(config_path.read_text())["servers"][1]["ignored"] == ["OtherOnly"]
                            # Cancellation discards additions and selected removals.
                            open_ignore()
                            add_name("Mallory")
                            assert Atspi.Component.grab_focus(checkbox("#red"))
                            key("space")
                            action("Remove ignore")
                            action("Cancel")
                            assert sorted(saved_ignores()) == ["#red", "BOB"]
                        else:
                            assert not checked("#red") and not checked("BOB"), "reopened removal checks should be unchecked"
                            action("Cancel")
                            wait_for(lambda: "saved Alice #blue" in output(), "allowed history missing")
                            assert "hidden saved Bob" not in output(), "ignored nickname leaked through history"
                            assert "saved Alice #red" not in output(), "ignored channel leaked through history"
                            command("/query Bob")
                            wait_for(lambda: "saved outgoing to Bob" in output(), "ignore hid our outgoing query history")
                            assert "hidden saved Bob private" not in output(), "ignored private history leaked"

                        # Select #blue so allowed live chat also exercises the speech path.
                        for _ in range(1 + sum(n.get_role() == Atspi.Role.PAGE_TAB for n in nodes())):
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
                            for _ in range(1 + sum(n.get_role() == Atspi.Role.PAGE_TAB for n in nodes())):
                                if any(n.get_role() == Atspi.Role.PAGE_TAB and n.get_name().startswith("#red")
                                       and n.get_state_set().contains(Atspi.StateType.SELECTED) for n in nodes()):
                                    break
                                key("ctrl+Tab")
                            assert any(n.get_role() == Atspi.Role.PAGE_TAB and n.get_name().startswith("#red")
                                       and n.get_state_set().contains(Atspi.StateType.SELECTED) for n in nodes()), "could not select #red"
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

                        # The same names stay visible and sendable on another server.
                        key("ctrl+Next")
                        open_ignore()
                        assert not checked("OtherOnly"), "current server was not selected in the combo box"
                        assert not any(n.get_role() == Atspi.Role.CHECK_BOX
                                       and n.get_name() in ("BOB", "#red")
                                       for n in walk(named("Ignore"))), "server-specific list leaked"
                        action("Cancel")
                        before_other = len(other.sent_messages)
                        command("/quote TEST-OTHER")
                        wait_for(lambda: "visible Bob other server" in output()
                                 and "visible red other server" in output(), "ignores leaked into another server's incoming messages")
                        command("/msg Bob allowed-other-person")
                        command("/msg #red allowed-other-channel")
                        command("/quote TEST-SENT other")
                        wait_for(lambda: "send barrier other" in output(), "other server barrier missing")
                        sent_other = other.sent_messages[before_other:]
                        assert len(sent_other) == 3, sent_other
                        assert any("allowed-other-person" in line for line in sent_other), sent_other
                        assert any("allowed-other-channel" in line for line in sent_other), sent_other
                        assert any(line.startswith("NOTICE Bob ") and "VERSION" in line for line in sent_other), sent_other
                        key("ctrl+Prior")

                        if attempt == 1:
                            open_ignore()
                            # Checking names alone does not unignore them.
                            assert Atspi.Component.grab_focus(checkbox("BOB"))
                            key("space")
                            action("OK")
                            assert sorted(saved_ignores()) == ["#red", "BOB"]
                            open_ignore()
                            assert Atspi.Component.grab_focus(checkbox("BOB"))
                            key("space")
                            key("Tab")
                            assert checkbox("#red").get_state_set().contains(Atspi.StateType.FOCUSED), "Tab should reach the next ignored name"
                            key("space")
                            key("Tab")
                            assert named("Remove ignore").get_state_set().contains(Atspi.StateType.FOCUSED), "Tab should reach Remove ignore"
                            key("Return")
                            action("OK")
                            assert saved_ignores() == []
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
                        if sys.exc_info()[0] is not None:
                            log.seek(0)
                            print(log.read(), flush=True)
                        if client.poll() is None:
                            client.terminate()
                            client.wait(timeout=5)
            print("PASS: per-server ignore controls, batch removal, live/history filtering, blocked sends, automatic replies, silence, cancel and restart")
        finally:
            server.shutdown()
            other.shutdown()


if __name__ == "__main__":
    run()
