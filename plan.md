# irctest — Implementation Plan

An accessible IRC client for Windows, built with wxDragon (GUI) and prismer (screen reader speech).

## Goals (from requirements)

- wxDragon GUI, using the git versions via `[patch.crates-io]`.
- Configuration in `irctest.toml` in the current directory.
- Server manager dialog: Connect / Add / Edit / Delete (Connect first). Per-server auto-connect.
- Server fields: host, port, TLS, server password, nickname (blank = global nickname), commands to send after connect.
- A place to set the global nickname.
- Notebook with tabs, ordered: server tab, then its channels, then its queries.
- Every tab: multi-line read-only output box and a single-line input box.
- Incoming text is appended to the end of the output box **without moving the cursor**.
- Speech through prismer: on by default for each new channel. F5 toggles speech for the current tab, Shift+F5 toggles speech everywhere.
  Spoken format: `"<nick> on <channel without #> says <text>"`.
- SASL comes later.

## Architecture

```
src/
  main.rs          start-up: load config, build main frame, auto-connect
  config.rs        Config / ServerConfig, load + save irctest.toml (serde + toml)
  net.rs           tokio runtime + one task per server wrapping irc::client::Client
  format.rs        irc::proto::Message -> display line / speech text
  ui/
    mod.rs
    main_frame.rs  frame, menus, notebook, routing events to tabs
    tab.rs         one tab: output TextCtrl + input TextCtrl + speech flag
    server_dialog.rs   server list (Connect/Add/Edit/Delete)
    server_edit.rs     add/edit one server
    settings_dialog.rs global settings (nickname, maybe username/realname)
  speech.rs        wrapper around prismer, global + per-tab on/off
  commands.rs      parse input-box text (/join, /msg, ...) into actions
```

### Threading

- **GUI thread** owns every widget, the prismer backend and all application state (tabs, speech flags).
- **Networking uses the [`irc`](https://crates.io/crates/irc) crate (1.1.0)**, which is async and needs tokio. A multi-threaded tokio runtime is created in `main` before `wxdragon::main` and lives for the whole program; the GUI keeps its `Handle` to spawn work.
- **One tokio task per connected server.** It builds an `irc::client::data::Config`, calls `Client::from_config(cfg).await`, then `client.identify()`, then loops over `client.stream()` and forwards each `Message` to the GUI.
- **GUI → network:** the GUI holds a clone of the client's `irc::client::Sender` (it is `Clone` and thread-safe, and sending is synchronous), so typed messages are sent straight from the GUI thread with `send_privmsg`, `send_join`, `send(Command::Raw(..))` and so on. No extra channel is needed.
- **Network → GUI:** post a closure with wxDragon's `call_after` (check exact name in `docs/events.md` during step 0). Fallback if that doesn't work well: an `mpsc::Receiver` drained by a wx `Timer` every ~50 ms.
- Events: `Connected`, `Message(irc::proto::Message)`, `NickChanged(String)`, `Disconnected(reason)`. Each carries a `ServerId` so the GUI knows which server tab group it belongs to.

### What the `irc` crate does for us

- Parsing and serialising messages (`irc::proto::Message`, `Command` enum with `PRIVMSG(target, text)` etc., tags, `source_nickname()`, `response_target()`).
- Registration: `identify()` sends `CAP END`, `PASS` (when the password isn't empty), `NICK`, `USER`.
- Replies to server `PING`s and sends its own; a dead connection ends the stream with `Error::PingTimeout` (`ping_time` / `ping_timeout` in `Config`).
- Nick in use (`433`): tries each entry in `alt_nicks` in turn, then fails with `Error::NoUsableNick`. We'll fill `alt_nicks` with `nick_`, `nick__`, `nick___`.
- Tracks our current nick: `client.current_nickname()`.
- CTCP replies (`VERSION`, `PING`, `TIME`, `SOURCE`, `USERINFO`, `FINGER`) with the `ctcp` feature.
- TLS: `use_tls`, with Windows' native TLS (`tls-native`, the default) or rustls (`tls-rust`).
- Outgoing flood throttling (`burst_window_length`, `max_messages_in_burst`).

### What we still do ourselves

- **Connect commands:** the crate can auto-join `channels` from its config after the MOTD (`376`/`422`), but we want arbitrary raw lines. We leave `Config::channels` empty and send `connect_commands` ourselves when we see `001` (`RPL_WELCOME`), using `Command::Raw`.
- **Our nick for mentions and routing:** `current_nickname()` lives on the `Client`, which stays inside the task. After each message the task compares it with the last value and sends `NickChanged` to the GUI when it changes.
- **Reconnecting:** the crate doesn't reconnect. When the stream ends or errors, the task reports `Disconnected` and a new `Client` has to be created (step 8).
- **Channel membership:** for routing other users' `QUIT`/`NICK` to the right tabs we keep our own per-channel nick sets. (The crate's `channel-lists` feature tracks users too, but only on the `Client` inside the task, so it's not easily usable from the GUI.)

### Things to check early (steps 0 and 3)

- **Bad bytes:** confirm that a line which isn't valid UTF-8, or fails to parse, doesn't end the stream. Keep the `encoding` feature on and set `encoding = "UTF-8"`. If bad lines still kill the connection, that's a reason to reconsider the crate. **Checked in step 3: invalid UTF-8 and junk lines both leave the connection up.**
- **Maintenance:** 1.1.0 was released in March 2025, 1.0.0 in March 2024. It's widely used and stable, but fixes may be slow.

### Config file shape

```toml
nickname = "tyler"
username = "tyler"      # optional, defaults to nickname
realname = "Tyler"      # optional

[[servers]]
name = "lan"
host = "10.0.0.2"
port = 6667
tls = false
password = ""           # ZNC: "user/network:pass"
nickname = ""           # blank = global
auto_connect = true
connect_commands = [
  "JOIN #test",
]
```

Saved whenever the server or settings dialogs are closed with OK. Missing file = empty default config (no error).

### Tab model

`Vec<TabInfo>` kept in the same order as the notebook pages:

```rust
enum TabKind { Server, Channel(String), Query(String) }
struct TabInfo { server: ServerId, kind: TabKind, speech: bool, output: TextCtrl, input: TextCtrl }
```

Insert position for a new tab: after the server's tab, after its last channel (for a channel) or after its last query (for a query). Names are compared case-insensitively (RFC 1459 casemapping is fine to start with ASCII lowercase).

### Appending without moving the cursor

In `tab.rs`:
1. Save `get_insertion_point()` and the selection.
2. `append_text(line)` (this moves the insertion point on Windows).
3. Restore the insertion point/selection.

Output box style: multi-line, read-only, `TE_RICH2` (avoids the 64 KB limit of the plain Windows edit control). Check with NVDA/JAWS that the cursor really stays put and nothing is read out on append. If wx still scrolls or announces, try `freeze()/thaw()` around the append.

### Speech

- `Speech` struct: `prism: prismer::Prism`, `backend` from `prism.create_best()`, `global_enabled: bool`.
- Speak only when `global_enabled && (tab.speech || mentions_me)`, where `mentions_me` means the message contains our current nick as a whole word (case-insensitive). Global off always wins.
- New channel and query tabs both start with speech on.
- `backend.speak(text, false)` (no interrupt) so fast traffic queues instead of cutting itself off.
- If prismer fails to initialise, log to the server tab and keep running without speech.
- Toggles are menu items with accelerators so they're discoverable from the menu:
  `View → Toggle speech for this tab\tF5`, `View → Toggle all speech\tShift+F5`.
  Each toggle speaks its new state once (e.g. "speech off for test", "all speech off") — the global-off message is spoken before muting.

## Steps

Each step ends with something runnable and tested against the LAN server (`10.0.0.2:6667`, no TLS).

### Step 0 — Project skeleton
- Add dependencies: `wxdragon`, `prismer`, `serde` (derive), `toml`, `tokio` (`rt-multi-thread`), `futures` (for `StreamExt`), plus the `[patch.crates-io]` block for wxDragon.
- `irc = { version = "1.1", default-features = false, features = ["ctcp", "tls-native", "encoding"] }`. This drops `toml_config` (we build `Config` in code) and `channel-lists` (see above).
- `main.rs`: open an empty frame with a menu bar (`Server`, `View`, `Help`) and a notebook.
- Confirm the wxDragon build works on this machine (the first build compiles wxWidgets and is slow).
- Confirm the API names we depend on: `call_after`, `Notebook`, `TextCtrl` styles, insertion point methods, menu accelerators, `Timer`.
- **Test:** `cargo run` shows the window; menus work with the screen reader.

**Done. Findings (wxDragon at git `e32baf6`, irc 1.1.0, prismer 0.1.4):**
- `wxdragon::call_after(Box<dyn FnOnce() + Send>)` only queues the closure; the queue is drained in the app's idle handler. Call `wxdragon::wake_up_idle()` after it so a quiet GUI processes it straight away. Widgets aren't `Send`, so the closure can't capture them: the network side pushes events into a channel and the posted closure drains it into GUI-thread state (e.g. a `thread_local!`).
- `Notebook` has `add_page`, `insert_page`, `remove_page`, `selection`, `set_selection`, `get_page`, but no `set_page_text` or `delete_page`; after `remove_page` the panel must be destroyed ourselves. wxWidgets' `wxNotebook::SetPageText` exists, but wxdragon-sys (upstream HEAD `e32baf6`) has no `wxd_Notebook_SetPageText` binding. **Decision:** rename tabs by calling `wxd_Treebook_SetPageText` (through a `wxdragon-sys` dependency) with the notebook's pointer. It does `((wxTreebook*)self)->SetPageText(..)`, a virtual call on `wxBookCtrlBase`, so it dispatches to `wxNotebook::SetPageText`. It works in practice but is technically undefined behaviour, so it lives in one small `unsafe` helper (`set_page_text` in `ui/`). If wxDragon later gains `wxd_Notebook_SetPageText`, switch the helper to it.
  (`AuiNotebook` has a proper binding, but it's custom-drawn and likely worse for screen readers; stick with `Notebook`.)
- `TextCtrl`: styles `MultiLine | ReadOnly | Rich2`, `ProcessEnter` + `on_text_enter`; `get_insertion_point` / `set_insertion_point`, `get_selection` / `set_selection`, `append_text`, `get_last_position` all exist.
- Menus: `Menu::builder().append_item(id, "Label\tAccel", help)`, `frame.on_menu_selected(|e: MenuEventData| e.get_id())`. F5 / Shift+F5 accelerators go in the label.
- `Timer::new(&owner)`, `on_tick`, `start(ms, one_shot)` exist if we need the fallback.
- `irc::proto::FormattedStringExt::strip_formatting` exists, but turned out to be incomplete (see step 2).
- **Bad bytes:** with the `encoding` feature, undecodable bytes are replaced, not an error. But a line that fails to *parse* (empty line, invalid command) returns an error from the codec, and tokio-util's framed stream ends after an error. So one malformed line could drop the connection; test it in step 3. (Step 3: the junk lines tried all parse as an empty raw command, so this didn't happen.)
- `build.rs` embeds a Windows manifest (common controls v6, UTF-8, per-monitor DPI) via `embed-manifest`, as the wxDragon examples do. Release builds use the Windows subsystem (no console).

### Step 1 — Config
- `config.rs` with `Config` and `ServerConfig`, `load()` / `save()` for `./irctest.toml`.
- `effective_nick(&ServerConfig)` returns the server nick or the global one.
- **Test:** unit tests for round-trip and missing-file default; hand-write an `irctest.toml` for the LAN server.

**Done.** All fields use `#[serde(default)]` (port defaults to 6667). `username` / `realname` are blank = use the nick (`effective_username` / `effective_realname`). A file that exists but can't be read or parsed shows an error and exits instead of starting with defaults, so a later save can't overwrite it. `save()` writes `irctest.toml.tmp` and renames it over the original. `irctest.toml` is in `.gitignore` since it can hold passwords. The Status page shows the server count and global nickname as a visible check.

### Step 2 — Message formatting
- `format.rs`: turn an `irc::proto::Message` into a display line and (for PRIVMSG/ACTION) speech text, with formatting codes stripped.
- Recognise `\x01ACTION ...\x01` in PRIVMSG text (the crate parses the line but leaves CTCP payloads in the text).
- **Test:** unit tests built from `"...".parse::<Message>()`: channel message, query, ACTION, NOTICE, colour codes.

**Done.** `format_message(&Message) -> Option<Formatted { text, speech }>`; `None` for PING/PONG/CAP. Speech is only set for PRIVMSG and ACTION (`"bob on test says hi"`, `"bob on test waves"`, privately `"bob says hi"` / `"bob waves"`); notices and other CTCPs are shown but not spoken. Also formats JOIN/PART/QUIT/NICK/TOPIC/KICK/MODE/INVITE/ERROR, numerics (dropping our nick from the first parameter) and falls back to the raw line. Channel names are spoken without their leading `#`s.
- Our own `strip_formatting` replaces the crate's: `FormattedStringExt` misses italics (`\x1D`), strikethrough (`\x1E`), monospace (`\x11`) and hex colours (`\x04`), and leaves digits behind for codes like `\x0304,05`.
- `mod format` had `#[allow(dead_code)]` in `main.rs`; removed in step 3 when it moved into the library.

### Step 3 — Connection task (no GUI yet)
- `net.rs`: `connect(handle, server_id, &ServerConfig, nick, event_sink)`; once the client is created, its `Sender` is handed to the GUI with the `Connected` event.
- Build `Config` from our config: `server`, `port`, `use_tls`, `password`, `nickname`, `alt_nicks`, `username`, `realname`, `encoding`, `version`.
- On `001`: send each `connect_commands` line as `Command::Raw`.
- Report `NickChanged` when `current_nickname()` changes; report `Disconnected` with the error text when the stream ends.
- **Test:** temporary `examples/headless.rs` that connects to 10.0.0.2, prints messages and joins a channel. Also try a nick that's already taken and a non-UTF-8 line from a second client. TLS gets tested against ZNC later (the LAN server is plain only).

**Done.** `config`, `format` and `net` moved into a library target (`src/lib.rs`) so the example can use them; the binary uses `irctest::...`.
- `net::connect(&Handle, ServerId, &Config, &ServerConfig, EventSink) -> AbortHandle`. `EventSink` is `Arc<dyn Fn(ServerId, NetEvent) + Send + Sync>`. Events: `Connected(Sender)`, `Message`, `NickChanged`, `Error` (non-fatal), `Disconnected(reason)` (always last; also sent for config problems like a missing nick).
- `net::send_raw(&Sender, line)` parses the typed line into a `Message` before sending, so `PRIVMSG #chan :hi there` is re-serialised correctly. Connect commands use it too.
- Unparseable lines (`InvalidMessage`) are reported as `Error` and the loop carries on; `NoUsableNick` and any other error end the connection.
- `examples/headless.rs` connects to a server from `irctest.toml` (first, or by name), prints lines and speech text, and sends stdin lines raw; EOF sends QUIT.
- **Tested against 10.0.0.2 (miniircd 2.3)** with a second raw client holding `tyler`: alt nick `tyler_` used, `JOIN #test` sent after 001, channel/action/colour/private messages formatted and given speech, a Latin-1 line arrived as `caf\u{FFFD}` and the connection carried on, a raw `PRIVMSG` from stdin reached the other client, QUIT closed cleanly.
- **Tested against a fake localhost server** sending an empty line, a prefix-only line and a whitespace-only line: these don't fail to parse; they become `Command::Raw("", [])`. The connection survives, and `format_message` now hides them.

### Step 4 — Server tab in the GUI
- `ui/tab.rs`: page panel with output (top, expands) and input (bottom). Enter in input → handler.
- Implement append-without-moving-cursor.
- `Server → Connect…` temporarily connects to the first configured server; all traffic/status goes to its server tab.
- Wire network → GUI with `call_after` (or the timer fallback); GUI → network through the stored `Sender`.
- Typing in the server tab: `/command args` is sent raw (`/raw` or `/quote` also accepted).
- **Test:** connect to 10.0.0.2, see MOTD; move the cursor up in the output while text arrives and check it stays put.

**Done.** `main.rs` builds a multi-threaded tokio runtime and passes its `Handle` to `MainFrame`. GUI state is `Rc<RefCell<State>>` (config, notebook, servers with tab / `Sender` / `AbortHandle` / nick). The network sink pushes `(ServerId, NetEvent)` into an `mpsc` channel, then `call_after` + `wake_up_idle`; the posted closure calls a `thread_local!` drain handler set by `MainFrame::new` (skipped via `try_borrow_mut` if already draining).
- `ui/tab.rs`: `Tab { panel, output, input }`, with "Output" / "Message" static labels before the controls for their accessible names. Tab order is tab control → Message → Output (Message is created first; the layout still puts Output on top). `append_line` saves the selection and insertion point, appends with a leading `\n` (so there's no empty last line), then restores them.
- `Server → Connect` (temporary) connects to the first server in the config, reusing its tab on reconnect. The "Status" placeholder page is removed when the first server tab is added.
- Server tab input: `/cmd args` is sent raw (`/raw` and `/quote` prefixes also accepted) and echoed as `-> cmd args`; plain text gets a hint.
- **Tested with UI Automation against 10.0.0.2:** MOTD and channel lines appear in the server tab; a caret placed on an earlier line and a selected word both stayed where they were while more lines arrived. UIA reports wx controls as generic panes, but the user confirmed tabs read correctly with the screen reader.

### Step 5 — Channels, queries, routing
- `JOIN` by us → create the channel tab (speech on) in the correct position. `PART`/`KICK` of us → note it in the tab (keep the tab so history stays). Rejoin reuses the existing tab.
- `PRIVMSG`/`NOTICE` to a channel → channel tab; to us → query tab for the sender (created if missing, speech on by default as well). Server notices / numerics → server tab.
- Other users' `JOIN`/`PART`/`QUIT`/`NICK`/`TOPIC`/`MODE` shown in the relevant channel tabs. (QUIT/NICK need channel membership: keep a simple per-channel nick set from `353` NAMES and JOIN/PART.)
- Output line format: `[HH:MM] <nick> text`, `* nick action`, `-nick- notice`. Use the `server-time` tag for the timestamp when present.
- `commands.rs`: plain text → `PRIVMSG <tab target>`; `/me`, `/join`, `/part [reason]`, `/msg nick text`, `/query nick`, `/nick`, `/quit [reason]`, `/close` (part and remove the tab); anything else sent raw.
- Our own messages are echoed into the tab (unless the server supports `echo-message` — skip for now).
- Selecting a tab does **not** move focus to its input box (that would break arrowing across the tab control). Focus only moves when the user opens a tab (`/join`, `/query`) or closes the focused one.
- **Test:** two clients on the LAN server talking in a channel and in private.

**Done.**
- `src/commands.rs`: `parse_input(line) -> Option<Input>` (`Say`, `Me`, `Msg`, `Join`, `Part`, `Query`, `Nick`, `Quit`, `Close`, `Raw`, `Usage`). `//text` sends `/text`; unknown `/cmd args` is sent raw.
- `src/route.rs`: `route(&Message, me) -> Route` (`Server`, `Channel`, `Query`, `TabIfOpen`, `UserTabs`) plus RFC 1459 `irc_eq` / `irc_lower`. Notices go to the sender's query tab if open, else the server tab; non-ACTION CTCP goes to the server tab; numerics whose second parameter names an open tab (topic, names, "no such nick") go there.
- `src/ui/state.rs`: per server, `tabs: Vec<ChatTab>` in notebook order (server, channels, queries); page index = earlier servers' tab counts + local index. Channel tabs track `joined` and an `irc_lower` nick set (from JOIN/PART/KICK/QUIT/NICK and 353) to route QUIT and NICK. Tabs created by incoming traffic aren't selected; a JOIN is selected only if we asked for it with `/join`. Disconnect marks all channels parted and is shown in every tab of that server.
- Lines are `[HH:MM] text`, using the `server-time` tag when present (`format::message_time`, `chrono`). Topic/names numerics are formatted ("Topic for #x: ...", "Users on #x: ..."); 366 is hidden.
- Our own messages are echoed by formatting a PRIVMSG with our nick as the source (so they look like everyone else's); `/msg` to a target without a tab is echoed as `-> target: text` in the current tab. JOIN/PART/NICK aren't echoed (the server's reply shows them).
- Query tabs aren't renamed on nick change yet (step 8), so messages from the new nick open a new query tab.
- **Tested against 10.0.0.2** with two raw clients (tab labels read via `TCM_GETITEM`): tab order `lan, #test, #second, bob, carol`; `/join` and `/query` select their tab, incoming traffic doesn't; channel text, `/me` and `/query` text arrive; QUIT and NICK show in the right query tabs; `/close` removes the tab and focus moves to the newly shown page.

### Step 6 — Server manager and settings dialogs
- `Server → Manage servers…` (Ctrl+S or similar): list box of servers + buttons **Connect, Add, Edit, Delete, Close**. Connect is the default button when a server is selected; Enter on the list connects.
- Add/Edit dialog: Name, Host, Port, TLS checkbox, Server password (password field), Nickname (with hint "blank = global"), Auto-connect checkbox, Connect commands (multi-line, one per line).
- `Server → Disconnect` for the current tab's server; `Server → Settings…` for the global nickname / username / realname.
- On start-up, connect every server with `auto_connect = true`.
- Every control has a label and a sensible tab order (use `StaticText` before each field so screen readers pick the label up).
- **Test:** add/edit/delete servers, restart, check `irctest.toml` and auto-connect.

**Done.**
- `ui/server_dialog.rs`: `manage_servers(parent, &Config, save) -> ManageResult { servers, connect }`. List entries read "lan (10.0.0.2:6667, TLS, auto-connect)". Tab order: list, Connect, Add, Edit, Delete, Close. Connect is the default button, so Enter on the list connects (double-click too); Connect/Edit/Delete are disabled with nothing selected. Delete asks Yes/No (No is the default). The list is saved to `irctest.toml` after every add, edit and delete rather than on close.
- `ui/server_edit.rs`: Name, Host, Port, Use TLS, Server password (password field), Nickname, Connect automatically at startup, connect commands (multi-line, one per line); OK is the default button, Esc cancels. Checks on OK: host required, port 1–65535, name unique (blank name = host).
- `ui/settings_dialog.rs`: Nickname, Username, Real name (blank = nickname); saved on OK.
- Server menu: Manage servers (Ctrl+Shift+S), Disconnect (current tab's server: QUIT if connected, cancels a connect in progress), Settings, Exit. The temporary Connect item is gone.
- `State::connect(ServerConfig)` reuses a server's tabs by name; new settings apply to a server the next time it connects. Auto-connect runs after the frame is shown, then selects the first server tab.
- State is never borrowed while a dialog is open, since network events are handled inside the dialog's event loop.
- **Tested via UI Automation + keyboard** (config backed up and restored): startup auto-connect; manager focus and tab order; Alt+A, fields filled by keyboard, Enter on OK → saved to `irctest.toml`; Enter on the list connected the new server with its own nick and connect commands; Disconnect; Settings shows current values and Esc closes it.

### Step 7 — Speech
- `speech.rs` as described above; F5 / Shift+F5 menu accelerators.
- Channel `PRIVMSG` → `"<nick> on <channel without leading #> says <text>"`. ACTION → `"<nick> on <channel> <action>"`. Query → `"<nick> says <text>"` (privately).
- Messages mentioning our nick are spoken even when that tab's speech is off, but not when global speech is off.
- Our own messages are spoken too (user decision), using the same format and tab speech setting; saying our own nick doesn't count as a mention.
- **Test:** with NVDA running, talk from a second client; toggle F5 on one channel and Shift+F5 globally.
- **Test (mentions):** turn a channel's speech off with F5, mention your nick from a second client and check it's spoken; then Shift+F5 and check it's silent.

**Done.**
- `src/speech.rs`: `Speech` holds a leaked (program-lifetime) `Prism` and a `Backend<'static>` from `create_best()`; on this machine that's NVDA. prismer uses its `static` feature, so there's no prism DLL to ship.
- Always the best backend: the `Prism` is built with an `on_availability` callback, which makes prism poll backend availability every second (2 agreeing samples to confirm a change) on its own thread. The callback only sets an `AtomicBool`; before the next speech, `create_best()` is called again. If a `speak` fails before the poll notices (NVDA just closed), it re-picks and retries once. So restarting NVDA goes NVDA → SAPI → NVDA without restarting the app. Startup with no backend at all also recovers once one appears.
- `should_speak(global, tab, mentions_me)` and `mentions(text, nick)` (whole word, RFC 1459 case-insensitive, formatting stripped) are pure and unit-tested.
- Spoken: PRIVMSGs and ACTIONs in channel and query tabs, including our own ("tyler on test says test"). Not spoken: notices, CTCP, joins/parts etc.
- Errors from typed input (plain text in a server tab, usage messages, "Not connected.", "Couldn't send", "Not in a channel", etc.) are written to the tab and spoken (`State::complain`), interrupting and even with all speech off, like toggle feedback. Echoes of sent commands (`-> WHOIS bob`) aren't spoken.
- Enter in an input box no longer beeps: wxDragon marks every event as skipped before calling a handler, so the Enter handler calls `event.skip(false)` to stop it reaching the edit control's default handling.
- F5 toggles the selected tab and says "Speech off for test" (or "Server tabs aren't spoken."); Shift+F5 says "All speech off" / "All speech on". These interrupt queued speech and are spoken even with all speech off. Messages queue (no interrupt).
- If prism can't start, "Speech isn't available: ..." is written once in the first server tab, and toggle feedback is written to the tab instead of spoken.
- Debugging: with `IRCTEST_SPEECH_LOG` set, speech is printed to stdout (`SPEAK: ...`) instead of spoken.
- **Tested against 10.0.0.2** with `IRCTEST_SPEECH_LOG` and a second client: channel message/action and private message spoken in the right format; notice not spoken; after F5, a plain message isn't spoken, "Tyler: are you there?" is, "tylers everywhere" isn't; after Shift+F5 neither mentions nor private messages are spoken; turning both back on restores speech.

### Step 8 — Polish
- Reconnect on disconnect (with back-off) by building a new `Client` and replacing the stored `Sender`; menu item to reconnect now. Don't auto-reconnect after `/quit` or `NoUsableNick`.
- Update tab labels on nick change for queries (via the `set_page_text` helper); show topic on join (`332`).
- Ctrl+Tab / Ctrl+Shift+Tab, Ctrl+F4 to close a tab. (No Page Up/Down scrolling from the input box: not useful with a screen reader.)
- Input history (Up/Down in input box).
- Limit output buffer size to 10,000 lines.

**Done.**
- Reconnect: `NetEvent::Disconnected { reason, retry }`; `retry` is false for config problems (no nick/host) and `NoUsableNick`. On a retryable drop, unless the user quit (`/quit`, `/raw QUIT`, Server → Disconnect), the server tab says "Reconnecting in N seconds..." and a per-server one-shot wx `Timer` reconnects after `net::reconnect_delay(attempt)`: 5 s doubling to a 5-minute cap, reset when 001 arrives. Reconnects use the server's latest settings from the config. Server → Reconnect drops the current connection (if any) and connects now; Server → Disconnect also cancels a pending reconnect. Drop reasons include the error's causes ("an io error occurred: An existing connection was forcibly closed...").
- Query tabs follow nick changes (`set_page_text` in `ui/tab.rs`, via `wxd_Treebook_SetPageText` as decided in step 0), unless the new nick already has a tab. The server only sends NICK for people sharing a channel with us. The topic on join was already shown in step 5 (332/333).
- Ctrl+Tab / Ctrl+Shift+Tab already worked: wx's notebook handles them, wraps, and focuses the new page's Message box. Ctrl+F4 is View → Close tab, same as `/close`.
- Input history (`src/history.rs`, per tab, 100 entries, no blanks or immediate repeats): Up/Down in the Message box. The draft being typed comes back after the newest entry. The recalled text isn't spoken by us; NVDA reads it.
- Output is capped at 10,000 lines per tab (`MAX_LINES` in `ui/tab.rs`); the oldest lines are removed and the caret/selection shift with the text so they stay on the same words.
- **Tested:** fake localhost server dropping the first connection → reconnected 5 s later, registered, and `/quit` didn't reconnect. LAN: bob's query tab renamed to bobby on NICK and bobby's next message used it; Up/Up/Up/Down/Down recalled "second line", "first line", (nothing older), "second line", "draft"; Ctrl+F4 closed `#test` and refused the server tab. With `MAX_LINES` temporarily 20: 35 lines left exactly 20, and the caret stayed on the same line.

### Step 9 — Logging
- Path: `logs/<server>/<target>/YYYY-MM-DD.log`, relative to the current directory (like `irctest.toml`).
  - `<server>` is the server's `name` from the config.
  - `<target>` is the channel name (with its `#`), the other person's nick for a query (routing already gives `bob` both for bob's messages and our echoed ones to him), or `server` for the server tab.
  - The date is the local date of the message's time (server-time tag if present, else now), so ZNC playback lands in the right day's file and files roll over at local midnight.
- `sanitize(name)`, applied to both the server name and the target:
  - `< > : " / \ | ? *` and control characters (0–31) become `_` (`bob|away` → `bob_away`).
  - Trailing dots and spaces become `_` (covers `.` and `..`).
  - Reserved device names (`CON PRN AUX NUL COM1–9 LPT1–9`, any case, also with an extension like `nul.foo`) get a `_` appended. `nul` and `con` are valid nicks.
  - An empty name becomes `_`.
  - Accepted collisions: `bob|x` / `bob_x` share a folder. `#Test` / `#test` share one too (Windows ignores case), which is what we want. A query with someone nicked `server` shares the server tab's folder.
- What's logged:
  - Every line that goes through `State::line_at`, as shown in the output box but stamped `[HH:MM:SS]`, and run through `strip_formatting` (most text is already stripped by `format_message`, but a few lines like `-> bob: text` skip it).
  - `/msg bob hi` with no bob tab open is also logged as a normal line in bob's query log, so the logger is keyed by (server, target), not by tab.
  - Toggle feedback written in place of speech (`feedback()`) isn't logged.
- `src/log.rs`:
  - `sanitize` and `log_path(server, target, date)`, both unit tested.
  - `Logger` holds open files keyed by path with the date each was opened for. `write(server, target, time, line)` creates folders as needed, opens files in append mode, reopens on a date change, and flushes every line.
  - Handles are dropped when a tab closes or a query is renamed on a nick change, so later lines go to the new nick's folder.
- Errors: the first write failure is shown in the server tab ("Logging failed: ..."). Later failures are silent, and logging keeps trying.
- Config: global `log = true` (default on), a "Log conversations" checkbox in the settings dialog.
- `.gitignore`: add `/logs`.

**Done.**
- `src/log.rs`: `sanitize`, `log_folder`, `log_path`, `Logger` (`write` / `close` / `close_all`), with unit tests for names, reserved names, paths, the day rollover, case-insensitive folders and colour stripping.
- `State::line_at` shows and logs. `show` only displays (toggle feedback, the "Logging failed" note). `log` writes when `config.log` is on. `remove_tab` and `rename_tab` close the tab's file. Turning logging off in Settings closes every file.
- `Config::log` defaults to true (a manual `Default` impl, also used for fields missing from the file). Settings has a "Log conversations" checkbox.
- **Tested:** the app against a fake localhost server, with the server named `fake:net`. It wrote `logs/fake_net/{server,#Test,bob,bob_away,nul_}/2026-10-13.log`. Colour and bold codes were stripped, the ACTION went to bob's query log, and "Disconnected" went to every tab's log. Not tested yet: `/msg` without a tab, and the Settings checkbox.

### Later
- SASL PLAIN. The crate has no built-in SASL, so we'd send `CAP REQ :sasl` / `AUTHENTICATE` ourselves and replace `identify()` with our own registration sequence (it sends `CAP END` immediately).
- Nick list, tab completion of nicks.
- Request `server-time` and suppress speech for ZNC playback buffer.

## Decisions

1. **ZNC buffer playback:** deferred. Playback is spoken like any other message for now (see "Later").
2. **Queries:** speech starts on for new query tabs, same as channels.
3. **Mentions:** messages mentioning your nick are spoken even when the tab's speech is off, but not when global speech is off. Notices get no special treatment.
