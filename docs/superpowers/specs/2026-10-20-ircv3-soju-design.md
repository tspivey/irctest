# IRCv3 and soju integration

Date: 2026-10-20
Status: Implemented and automatically verified on 2026-10-21; live and native accessibility acceptance remains pending.

## Intent and acceptance

The user requested IRCv3 and soju extensions and confirmed automatic network
connections, chat history, and synchronized read markers. They approved extending
the existing connection and tab system, quiet history retrieval, `/history`, and
explicit `/markread` control.

Success means a user can save one soju account, connect, and use its networks in
separate server/channel/query tabs. Reconnecting restores useful conversation
context without duplicate message display, historical speech, or changes to live
channel membership. Other devices can update the user's read position. Direct
IRC connections and existing configuration files continue to work.

This specification defines the supported extensions explicitly; it does not claim
support for every IRCv3 or soju extension. File hosting, web push, search, metadata,
typing indicators, reactions, message editing, SASL EXTERNAL, and a graphical
bouncer administration interface are outside this change. Existing raw commands
and BouncerServ remain available for network administration.

## Current implementation

- `src/net.rs` runs one Tokio task per connection. CAP negotiation currently runs
  only when a SASL password exists and only requests SASL PLAIN. The network task
  answers CTCP queries and reports parsed messages to the GUI.
- `src/ui/state.rs` owns connections, channel membership, reconnect timers, tabs,
  routing, local outgoing echoes, logging, and speech decisions.
- `src/format.rs` already parses `time` tags. `src/route.rs` already handles
  private messages sent by the current user through a bouncer.
- `src/ui/tab.rs` appends text while preserving the output selection and cursor.
  Each tab retains at most 10,000 output lines.
- `src/config.rs` uses serde defaults and atomic replacement when saving TOML.
  `src/history.rs` is typed-input recall and must keep that responsibility.

## Approach and ownership

Extend the existing `irc-proto`, Tokio, wxDragon, and TOML architecture. Keep
protocol state independently testable in the library instead of embedding it all
in the GUI state. No replacement IRC library or new runtime is required.

- A capability module owns offered, pending, and enabled capabilities and the
  registration barrier. `Connection` owns SASL exchange and wire I/O.
- A bouncer module owns network attributes and reconciliation keyed by parent
  account plus opaque network ID. The GUI owns child connection lifetimes.
- A playback module owns batch ancestry, history requests, selectors, bounded
  deduplication, and message origin classification.
- A synchronization-state module owns persistent conversation cursors and read
  markers. It stores no passwords or message bodies.
- The GUI owns display, focus, tab labels, explicit user actions, and the decision
  to speak. It consumes protocol events before ordinary formatting/routing.

Changes to `NetEvent` must update all consumers, including `examples/headless.rs`.
Keep ordinary `NetEvent::Message` delivery for live messages; introduce explicit
capability, network, playback, and read-marker events where required. Avoid
representing playback status with synthetic IRC message tags.

## Capability negotiation and outgoing messages

Send `CAP LS 302` on every connection before PASS/NICK/USER. Collect multiline LS,
preserve capability values, and request only supported advertised capabilities.

The supported set is `sasl` (PLAIN, when configured), `cap-notify`, `server-time`,
`message-tags`, `batch`, `echo-message`, `multi-prefix`, `extended-join`,
`userhost-in-names`, `draft/chathistory`, `draft/read-marker`,
`soju.im/bouncer-networks`, and `soju.im/bouncer-networks-notify`.

Use the published draft capability names. Request history only when its batch,
timestamp, and tag dependencies can also be enabled. Request network
notifications only with bouncer networks and batch support. Request read markers
only with server timestamps. Do not request event playback; nevertheless classify
unexpected historical state-changing messages safely.

Automatic discovery requires configured SASL PLAIN credentials so children can
authenticate before binding. Negotiate the bouncer capabilities only when
discovery is enabled and those credentials are available, or when explicitly
binding a discovered child. PASS-only setups retain their existing manually bound
network behavior; document SASL as a prerequisite for automatic discovery.

Track each CAP request until ACK or NAK, keep lines within the IRC command limit,
and finish registration only after outstanding requests and SASL complete.
Handle partial ACKs, rejected request groups, and dynamic NEW/DEL. On grouped NAK,
retry each eligible capability once individually; never retry indefinitely.
Capability removal disables dependent behavior and clears affected requests.

An early welcome or unknown-CAP response permits ordinary registration. Bound
negotiation and authentication waits to 30 seconds. A stalled exchange produces
one explanation and closes the connection through existing reconnect handling.
Preserve existing direct-server behavior after an explicit SASL failure: report
it and continue unauthenticated. A discovered child requiring BIND must instead
stop if authentication or its bouncer capability fails; never let that child
become an unbound connection.

Expose enabled capabilities before welcome handling or any echoed chat event.
When echo-message is enabled, display/log outgoing PRIVMSG, ACTION, and NOTICE
from the server echo only. Otherwise retain local echo. Apply the rule to normal
input, `/msg`, `/query` text, `/me`, and raw chat commands. Echoes from another
device are displayed once and retain the existing speech policy for live own
messages. Historical own messages remain quiet.

Unknown well-formed tags are ignored. CAP, BATCH, TAGMSG, AUTHENTICATE, BOUNCER,
MARKREAD, and history control messages must not appear as raw conversation text.
Format FAIL/WARN/NOTE as readable replies, with errors reaching the appropriate
server or requesting conversation tab.

## Automatic soju network connections

Add a per-server `discover_networks` boolean, default true, and an accessible
checkbox labeled "Discover bouncer networks" in the existing server editor.
False keeps manually configured bouncer/network connections available.

After registration and initial ISUPPORT, a connection with the bouncer capability
and no bound `BOUNCER_NETID` becomes the account's control connection. A connection
already bound by the user's existing username configuration remains an ordinary
network connection and never starts discovery. Wait for end of MOTD/no MOTD
before deciding this role; use a five-second fallback after welcome for servers
that omit both. Retain any early network notifications until the role is known.

Use the automatic network snapshot when notifications are enabled; otherwise
issue `BOUNCER LISTNETWORKS`. Accept only the appropriate control connection's
network events for child creation. Merge partial attribute updates, unescape tag
values, ignore unknown attributes, and interpret `*` as deletion. Network state
is keyed by stable network ID, never by display name.

Create one child per discovered network, connecting to the parent's bouncer
endpoint with the parent's account credentials and certificate policy. Never
connect directly to an advertised upstream host. After authentication and CAP ACK,
send `BOUNCER BIND <id>` before `CAP END`. Child connections must not recursively
discover more children. Run the parent's connect commands once on each child,
not on the control connection. Ordinary direct and manually bound connections
retain their existing connect-command behavior.

Display child server tabs as "account / network"; add the network ID when names
collide. Renaming a network updates labels without losing drafts, output,
membership, or identity. Use a stable ID-based namespace for child logs and state
so renames do not mix or split conversations. Do not persist children as separate
server configuration entries or duplicate account credentials.

Limit simultaneous child registrations to four. Queue additional networks;
cancel queued work if the account is disconnected. Upstream `state=disconnected`
is an informative status, not a reason to repeatedly recreate the downstream
connection. New discovery never changes focus or selects a tab.

Network removal cancels its task, pending registration, and reconnect timer; keep
its existing tabs with a clear removed status. Late events from the old task are
ignored. Each connection attempt has a generation token so stale events cannot
alter a replacement attempt. Repeated notifications never create duplicate tasks.

An accidental control-connection drop retries the control connection but leaves
healthy children running. Reconcile children when a complete new snapshot
arrives; do not infer deletion from an incomplete batch. Explicitly disconnecting
the account cancels all children and timers. Disconnecting one child suppresses
its automatic reconnection/discovery until explicit reconnect or a fresh user
connection to the account. Application shutdown cancels the entire group.

For discovered children, soju owns upstream channel restoration. Do not send the
client's ordinary automatic rejoin list on every downstream welcome. Keep direct
server rejoin behavior. Explicit user joins, parts, and connect commands still
work normally.

Certificate trust belongs to the saved parent account. Complete the control
connection's trust decision before starting children; share accepted fingerprints
within the group. Always verify each child's actual certificate. A mismatch stops
that attempt and uses the existing certificate prompt rather than assuming trust.

## History retrieval and display

Maintain a per-session batch table with parent links, type, target, and request
association. History classification follows ancestry, including nested batches.
Unknown batch types do not silently become history. Recognize `chathistory` and
the legacy `znc.in/playback` batch type as quiet historical delivery.

History must bypass live membership changes, current nickname changes,
auto-selection, mention announcements, CTCP replies, and ordinary speech. Apply
this separation in the network task as well as the GUI: old CTCP requests must
never receive new replies. BATCH controls remain invisible.

Once registration and ISUPPORT are ready, retrieve recent history on the user's
own JOIN and when opening a private conversation. Restore missed private
conversations using CHATHISTORY TARGETS; do not assume known channel tabs cover
all missed messages. Query only channels the client is joined to; discovering
history must not itself rejoin a parted channel.

Default page size is 100 messages, clamped to a positive advertised CHATHISTORY
limit. Respect MSGREFTYPES when choosing selectors; prefer IDs when supported and
available, otherwise timestamps. Use LATEST for initial context, BEFORE for older
pages, and AFTER for reconnect catch-up. Use a five-second timestamp overlap on
reconnect to account for equal timestamps and clock skew. Preserve message order
within returned batches; do not reorder equal timestamps arbitrarily.

Allow two concurrent history requests per connection and one per conversation.
Each request has a 30-second deadline. Limit automatic catch-up to 1,000 messages
per conversation per connection and initial TARGETS discovery to 100 targets.
Continue TARGETS pagination up to that bound. If a bound is reached, provide a
quiet notice that more history may be available through `/history`; never report
the history as complete. Stop on empty responses, end tags, no cursor progress,
errors, timeout, or capability removal.

`/history [count]` requests older history for the current channel/query; count is
1 through 1,000, default 100, further clamped to the server limit. On an empty tab,
retrieve latest history instead. Unsupported capability, server-tab use, pending
request, or disconnection gives a short actionable message. No new keyboard
shortcut is required.

Use message IDs for deduplication across live delivery, reconnect overlap, and
history pages. Keep a bounded cache of 4,096 IDs per conversation and persist
only the latest 256. For servers without IDs, use exact timestamp/source/command/
target/body tuples with occurrence counts limited to overlapping retrievals;
never deduplicate live messages against one another. Timestamp-only servers
cannot guarantee perfect overlap deduplication, which must be documented.

Buffer bounded history results before inserting them at the appropriate point
relative to existing chat lines. Keep live delivery responsive while a request is
pending. Preserve selection, insertion position, and reading context when
inserting older text or trimming output. Keep the existing 10,000-line bound.
Display historical dates as well as times so older days are distinguishable.

Log only messages newly accepted by deduplication during the current process,
using their original date/time. Explicit older-history retrieval does not append
to ordinary logs. On a cold start, restored context is display-only; subsequent
live messages and reconnect catch-up are logged normally. This avoids copying
startup context into existing log files without introducing a message database.

## Read markers and durable cursors

Handle `draft/read-marker` with MARKREAD. Retain a monotonically increasing
server timestamp per conversation; `*` means unknown and does not erase a known
marker. Query the initial marker for private conversations; channel markers are
received from the server. Incoming updates never generate a response loop.

Use explicit marking for this change. `/markread` marks the current conversation
through the latest displayed chat message that has a valid server `time` tag.
Do not substitute the local clock, claim an unknown timestamp, or send `*`.
Receiving messages, restoring history, selecting a tab, and queueing speech do
not themselves prove the user has read the messages and do not advance markers.
Document this rule in the command help. A successful mark gives short feedback;
an unavailable capability or missing server timestamp gives an explanation.

Channel/query labels include a plain-text unread indicator when a known received
message is newer than the shared marker. Remote marker updates can clear it;
changing labels must preserve identity, focus, and drafts. Label changes do not
cause extra speech. Historical unread messages remain available for manual
review without being automatically spoken.

Persist received-history cursors separately from read markers in versioned
`irctest-state.toml` next to the existing config. Receiving a message advances a
retrieval cursor, not its read marker. Identify an account by endpoint, TLS mode,
authentication identity, and configured account name; append opaque network ID
and IRC-folded conversation name. A changed account or endpoint must not inherit
another account's markers. For direct servers, omit the network ID.

Persist only cursor timestamps/IDs, bounded recent IDs, and read timestamps.
Store no password or message body. Use atomic replacement and a single serialized
writer with a two-second debounce plus shutdown flush. Save failures are reported
once and do not interrupt chat. A missing file is normal; invalid syntax or an
unsupported version leaves the original untouched and uses memory-only state.
Add generated state and temporary filenames to `.gitignore`.

## Resource limits, errors, and cancellation

Bound each connection to 128 active batches, nesting depth 16, and 1,000 buffered
messages per history request with a combined four-megabyte text limit. Preserve
the existing bounded network-line handling. On malformed or excessive batches,
report one protocol error and close/retry through existing connection handling;
never deliver ambiguous historical data as live activity.

Cancel per-session requests, batch state, authentication state, and pending
capability requests on disconnect. Keep only conversation cursors, read markers,
deduplication state, and user drafts across reconnects. Ignore stale attempt
events. A late closed history batch must not complete a newer request.

Errors identify the account/network or conversation and operation. Do not include
authentication payloads or password-bearing BOUNCER attributes in display, logs,
parse-error diagnostics, or headless traces. Generic unknown-command handling
must not leak raw protocol credential data.

## Verification and delivery

Use the existing in-memory Tokio duplex session harness for wire behavior and
pure Rust tests for reconciliation, request scheduling, origin classification,
read markers, persistence, and unread state. Avoid timing sleeps; use deterministic
events and paused Tokio time for deadlines.

Required automated cases:

- CAP without SASL; multiline LS and values; ACK/NAK groups; NEW/DEL; absent CAP;
  authentication success/failure/timeout; exactly one END; BIND ordering.
- Initial discovery, partial updates, deletion, duplicate names/notifications,
  parent reconnect, child opt-out, bounded fan-out, stale attempt events, and
  bound legacy connections without discovery.
- Nested/interleaved history batches; live traffic during retrieval; historical
  CTCP and membership isolation; history limits and timeout; missed private
  targets; equal timestamps; ID overlap; no-ID repeated identical live messages.
- Echo/local fallback for every outgoing path; one displayed/logged outgoing
  message; incoming own messages from another client.
- Monotonic remote markers, unknown markers, explicit mark commands, absence of
  server timestamps, unread transitions, no implicit marking, account isolation,
  atomic state round-trip, and invalid/unsupported state files.
- Quiet playback through every speech path, including server-tab speech and
  error speech; source dates in history; correct log behavior.

Run focused module tests, then `cargo test --workspace --all-targets`,
`cargo check --workspace --all-targets`, and formatting checks. If native GUI
dependencies prevent a check, report the exact limitation and obtain tool
approval for required dependency/network access. Do not replace a running client
or connect to an external account without authorization.

Manual acceptance remains separate from automated evidence: two-network soju
login, rename/removal while connected, reconnect with missed channel and private
messages, a second device updating markers, keyboard review during history
insertion, and screen-reader confirmation of quiet playback and stable focus.
Report any unperformed live, audible, or platform-specific checks honestly.

Update user-facing documentation with configuration examples, exact capability
coverage, `/history`, `/markread`, quiet playback, and history/deduplication
limits. Update the stale future-work entries in `plan.md` only when implemented.

## Protocol references

- [IRCv3 capability negotiation](https://ircv3.net/specs/extensions/capability-negotiation)
- [IRCv3 message tags](https://ircv3.net/specs/extensions/message-tags)
- [IRCv3 batch](https://ircv3.net/specs/extensions/batch)
- [IRCv3 echo-message](https://ircv3.net/specs/extensions/echo-message)
- [IRCv3 draft/chathistory](https://ircv3.net/specs/extensions/chathistory)
- [IRCv3 draft/read-marker](https://ircv3.net/specs/extensions/read-marker)
- [soju bouncer networks and notifications](https://github.com/emersion/soju/blob/master/doc/ext/bouncer-networks.md)

Protocol details were checked against these upstream specifications on
2026-10-20. Draft capabilities must remain prefixed until upstream ratifies a
compatible replacement.
