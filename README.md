# irctest

irctest is a native IRC client built with wxDragon and prismer speech. It has
server, channel, and private conversation tabs, keyboard commands, direct IRC
connections, and a supported subset of IRCv3 and soju extensions.

## Getting started

Run the client from the directory where you want its configuration, logs, and
synchronization state stored. Open Server, Manage servers (Ctrl+Shift+S) to add
and connect to a server. Server, Settings contains the global nickname, username,
real name, and logging option. Each server can override the nickname and connect
automatically at startup.

Every tab has a Message input and a read-only Output control. Enter sends input;
Up and Down recall previous input. Tab moves between controls. Ctrl+Tab selects
the next tab; Ctrl+Shift+Tab selects the previous tab, wrapping through all
servers, networks, channels and private conversations. Ctrl+PageDown and
Ctrl+PageUp jump to the next or previous server/account/network tab, skipping
its conversations. These actions are also in the View menu and focus the
destination's Message input, preserving any unsent draft. A soju account's
control connection and each discovered network have their own server tabs in
this same strip. Ctrl+F4 closes a conversation, parting a channel if necessary.
Server tabs remain open; use Server, Disconnect or `/quit` to disconnect.

F4 stops whatever is being spoken, for speech backends such as Speech
Dispatcher that don't stop when Ctrl is pressed. F5 toggles speech for the
current tab. Shift+F5 toggles all speech. New
conversation tabs start with speech enabled. With global speech enabled,
mentions can still be announced when that tab's speech is off. The client
starts each output cursor at the bottom, including restored startup history.
Once you focus Output to review it, the client preserves the cursor and selection
when appending messages and adjusts them when inserting history or removing old
lines. Each tab retains at most 10,000 output lines.

## Configuration

`irctest.toml` is loaded from the current working directory. A missing file gives
an empty server list. The server manager saves configuration; existing files
without newer fields remain supported. Configuration contains any passwords you
enter; synchronization state contains no passwords or message bodies.

For a direct IRC connection, an example complete configuration is:

```toml
nickname = "your-nick"
username = ""
realname = "Your name"
log = true

[[servers]]
name = "direct"
host = "irc.example.net"
port = 6697
tls = true
auto_connect = false
discover_networks = false
connect_commands = ["JOIN #example"]
```

An empty username or real name uses the effective nickname. The optional
per-server `password` field sends IRC PASS. For a server requiring SASL PLAIN,
set `sasl_username` and `sasl_password` as in the next example. A blank SASL
password disables SASL; a blank SASL username uses the nickname. Untrusted TLS
certificates use the existing fingerprint prompt with Trust once, Trust always,
or Disconnect choices.

For automatic soju network connections, use an unbound account login over TLS:

```toml
nickname = "your-nick"
log = true

[[servers]]
name = "my-soju"
host = "bouncer.example.net"
port = 6697
tls = true
auto_connect = false
discover_networks = true
sasl_username = "your-account"
sasl_password = "replace-with-your-password"
connect_commands = []
```

Automatic discovery requires configured SASL PLAIN credentials and advertised
bouncer capabilities. `discover_networks` defaults to true; clear Discover
bouncer networks in the server editor, or set it to false, to opt out.
An unbound bouncer login without SASL now explains why no network sessions were opened and points to the SASL fields and reconnect action.

PASS-only logins keep their existing manual network behavior and do not enable
automatic discovery. Existing network-qualified login configurations also
remain usable: a connection reported as bound through `BOUNCER_NETID` does not
start discovery.

The saved account becomes a control connection. Each discovered network gets a
separate connection to the same bouncer endpoint, authenticates, and binds to
its network before completing registration. Child tabs are named
`account / network`, with IDs distinguishing duplicate names. Children reuse
the account's credentials and certificate policy; they are not separate saved
servers. Connect commands run on each network connection, not the control
connection. soju restores upstream channel membership for discovered children.

Disconnecting an account cancels its children and queued connections.
Disconnecting a single child opts that network out until an explicit reconnect
or a fresh connection to the account. An accidental control connection drop
leaves healthy children running while the control connection reconnects.
Network rename/removal notifications update the existing tabs without selecting
them. Network administration remains available through raw IRC commands and
BouncerServ; there is no graphical bouncer administration interface.

## Commands

Plain text sends a message to the current conversation. Begin with `//` to send
text starting with `/`.

- `/join #channel [key]`, or `/j`, joins a channel.
- `/part [#channel] [reason]`, or `/leave`, leaves a channel.
- `/query nick [message]`, or `/q`, opens a private conversation.
- `/msg nick-or-channel message` sends a message directly.
- `/me action` sends an action to the current conversation.
- `/nick newnick` changes your nickname.
- `/history [count]` retrieves older messages for the current conversation.
- `/markread` explicitly marks displayed chat as read.
- `/close` closes a conversation tab.
- `/quit [reason]` disconnects the current server or account group.
- `/raw IRC command` and `/quote IRC command` send a raw IRC line. Unrecognized
  slash commands are also sent as IRC commands, for example `/whois nick`.

`/history` defaults to 100 messages. Counts must be 1 through 1,000 and are
clamped to the server's advertised limit. An empty conversation requests recent
history instead of an older page. Use it in a joined channel or private
conversation; a server tab, disconnection, unsupported history, or an existing
request gives an explanation. After a canceled or timed-out request, reconnect
before retrying that conversation so a late response cannot complete a newer
request.

`/markread` uses the newest valid server timestamp among displayed chat
messages. It requires `draft/read-marker`; without a server timestamp it gives
an explanation instead of using the local clock. It sends the marker and waits
for server confirmation. Receiving messages, restoring history, selecting a
tab, and queueing speech never advance the read marker. Shared markers move
forward; an unknown marker does not erase a known position. Conversation labels
show `[unread]` when received timestamped messages are newer than the known
marker, or when no marker is known. Updates from another device can clear it.

## History, logs, and stored state

Supported servers provide recent context when joining a channel or opening a
private conversation, and catch-up after reconnecting. TARGETS discovery can
restore missed private conversations without joining parted channels. Message
IDs are preferred for selectors; timestamp catch-up uses a five-second overlap.
Timestamp-based history stays available when soju withdraws `message-tags` for
an upstream network. After restarting the client, recent context is restored
even when catch-up has already returned messages; saved cursors do not replace
the conversation's displayed history.

History is quiet: it does not speak messages or mentions, change focus, update
live membership or nicknames, or answer old CTCP requests. Nested historical
batches retain that classification; legacy `znc.in/playback` is also recognized.
Historical lines include their original date and time. Live traffic continues
while history is being retrieved.

With logging enabled, direct conversations use
`logs/<server>/<target>/YYYY-MM-DD.log`. Discovered networks use a stable account
and network ID namespace so network renames do not split their logs. Newly
accepted live messages and reconnect catch-up are logged using their original
dates. Startup context and explicit older `/history` pages are display-only.
Deduplication avoids logging the same accepted message again within the process.

`irctest-state.toml`, next to the configuration, stores received cursors, recent
message IDs, and shared read timestamps. Received cursors and read markers are
separate: fetching a message does not mark it read. State is scoped by saved
account name, endpoint, TLS mode, authentication identity, network ID, and
conversation. Writes use atomic replacement, a single writer, a two-second
debounce, and a shutdown flush. Invalid or unsupported state is left untouched
and uses memory-only state for that run. Write failures are reported without
interrupting chat.

Passwords are excluded from state identity. If a PASS-only login changes account
solely by changing its password, the client cannot distinguish the old and new
identities. Give distinct accounts distinct saved server names. Servers without
message IDs cannot guarantee perfect overlap deduplication; timestamp/source/
command/target/body matching is limited to overlapping retrievals, and identical
live messages are never deduplicated against each other. Only recent IDs are
retained, so ID deduplication is also bounded rather than a permanent archive.

## Supported extensions and limits

The exact negotiated capability set is:

```text
sasl (PLAIN only, when configured)
cap-notify
server-time
message-tags
batch
echo-message
multi-prefix
extended-join
userhost-in-names
draft/chathistory
draft/read-marker
soju.im/bouncer-networks
soju.im/bouncer-networks-notify
```

Capabilities are requested only when advertised and eligible, with dependencies
required for history, read markers, and network notifications. With
`echo-message`, outgoing chat is displayed from the server echo; otherwise the
client supplies local echo. Dynamic capability removal disables dependent
behavior. This is a supported subset, not support for every soju or IRCv3
extension. File hosting, web push, search, metadata, reactions, message editing,
typing indicators, SASL EXTERNAL, event playback negotiation, and graphical
bouncer administration are outside its scope.

Resource limits are:

- Four simultaneous child registrations per account.
- Two active history requests per connection; one per conversation.
- Default history pages of 100 messages, at most 1,000 per requested page and
  at most 1,000 automatic catch-up messages per conversation per connection.
- At most 100 targets during initial history discovery.
- A 30-second negotiation/authentication deadline and 30-second history waits.
- At most 128 active batches per connection, nested at most 16 levels deep.
- At most 1,000 buffered messages per history request and a combined 4 MiB
  history text buffer per connection.
- At most 4,096 recent message IDs per conversation; the latest 256 persist.
- At most 10,000 displayed lines per tab.

Retrieval stops on exhaustion, errors, timeout, capability removal, or lack of
cursor progress. A reached catch-up or discovery bound gives a quiet notice
that more history may be available; it does not claim the archive is complete.
Malformed or excessive batches close the connection rather than turning
ambiguous history into live activity.

## Building and verification

Use a Rust toolchain compatible with the checked-in lockfile and Rust edition
2024, plus the native build dependencies needed by wxDragon, prismer, and TLS.
The Cargo patches use wxDragon from Git. Initial builds may fetch dependencies
and build the native GUI toolkit.

```sh
cargo build --locked
cargo run --locked
cargo test --locked --workspace --all-targets
cargo check --locked --workspace --all-targets
cargo fmt --all -- --check
```

Focused protocol and state checks can be run without starting a GUI or
connecting to a real account:

```sh
cargo test --locked --lib cap::
cargo test --locked --lib net::tests::
cargo test --locked --lib playback::
cargo test --locked --lib bouncer::
cargo test --locked --lib connections::
cargo test --locked --lib read_marker::
cargo test --locked --lib sync_state::
```

The Linux native navigation regression uses an isolated virtual display,
temporary configuration and logged speech. It requires Python GObject with
AT-SPI bindings, xdotool, D-Bus and Xvfb, and does not use a real IRC account:

```sh
cargo build --locked
dbus-run-session -- xvfb-run -a python3 tests/gui_navigation.py
dbus-run-session -- xvfb-run -a python3 tests/gui_soju_networks.py
dbus-run-session -- xvfb-run -a python3 tests/gui_history.py
```

The soju regression uses a local fake bouncer with five networks to check that
children started from the connection queue also receive their network labels.
The history regression checks first login and restart, chronological order,
duplicate suppression and quiet playback, with and without `message-tags`.

On Linux, the existing packaging task builds a release AppImage:

```sh
cargo xtask appimage
```

Its output is `target/release/irctest-<version>-<architecture>.AppImage`. It
bundles resolved application libraries while leaving system libraries such as
glibc and graphics drivers on the host.

Automated tests exercise models, fake in-memory IRC sessions and native GTK
tab/network keyboard navigation with focus and draft preservation. Live soju
SASL login, discovery of five networks, their labels, restored channel tabs,
and history retrieval were verified on Linux on 2026-10-21. Network rename/removal,
reconnect with missed messages, and marker synchronization from a second device remain
unperformed. Audible speech,
screen-reader review, keyboard/focus behavior during history insertion, and
Windows native selection/cursor behavior with Unicode also remain unperformed.


The existing checkout has formatting deviations in untouched Rust files. Changed
Rust files are checked separately; the repository-wide formatting command above
may still report those pre-existing differences.
