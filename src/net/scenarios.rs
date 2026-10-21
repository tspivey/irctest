//! Transcript checks using the actual session, scheduler, and bouncer group.
use super::*;
use crate::connections::{ChildAction, Group};
use crate::playback::HistoryReason;
use tokio::io::{DuplexStream, Lines, ReadHalf, WriteHalf};
use tokio::task::JoinHandle;

const CAPS: &str = "sasl cap-notify server-time message-tags batch echo-message draft/chathistory draft/read-marker soju.im/bouncer-networks";

fn account(attempt: u64, bind: Option<&str>) -> Registration {
    Registration {
        host: "saved-bouncer.example".into(),
        port: 6697,
        tls: true,
        password: String::new(),
        nick: "reader".into(),
        alt_nicks: vec!["reader_".into()],
        username: "account".into(),
        realname: "Screen Reader".into(),
        connect_commands: vec!["JOIN #configured".into()],
        sasl_username: "account".into(),
        sasl_password: "credential-secret".into(),
        pinned: "AA:BB".into(),
        accepted: vec!["CC:DD".into()],
        discover_networks: true,
        options: SessionOptions {
            attempt,
            bind_network: bind.map(str::to_owned),
        },
    }
}

struct Peer {
    id: ServerId,
    attempt: u64,
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    write: WriteHalf<DuplexStream>,
    events: mpsc::UnboundedReceiver<(ServerId, ConnectionEvent)>,
    task: Option<JoinHandle<(String, bool)>>,
    sender: Sender,
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Peer {
    async fn start(id: ServerId, reg: Registration) -> Self {
        let attempt = reg.options.attempt;
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (tx, mut events) = mpsc::unbounded_channel();
        let sink: EventSink = Arc::new(move |id, event| {
            tx.send((id, event)).unwrap();
        });
        let task = tokio::spawn(session(id, reg, client, sink));
        let (read, write) = tokio::io::split(server);
        let (actual_id, event) = events.recv().await.unwrap();
        assert_eq!(actual_id, id);
        assert_eq!(event.attempt, attempt);
        let NetEvent::Connected(sender) = event.event else {
            panic!("Connected must be first")
        };
        let mut peer = Self {
            id,
            attempt,
            lines: BufReader::new(read).lines(),
            write,
            events,
            task: Some(task),
            sender,
        };
        peer.expect("CAP LS 302").await;
        peer.expect("NICK reader").await;
        peer.expect("USER account 0 * :Screen Reader").await;
        peer
    }

    async fn send(&mut self, line: &str) {
        self.write
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .unwrap();
    }

    async fn line(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(2), self.lines.next_line())
            .await
            .expect("expected client command")
            .unwrap()
            .expect("session ended before expected command")
    }

    async fn expect(&mut self, expected: &str) {
        assert_eq!(self.line().await, expected);
    }

    async fn fence(&mut self, token: &str) {
        self.send(&format!("PING :{token}")).await;
        self.expect(&format!("PONG {token}")).await;
    }

    fn events(&mut self) -> Vec<NetEvent> {
        let mut events = Vec::new();
        while let Ok((id, event)) = self.events.try_recv() {
            assert_eq!(id, self.id);
            assert_eq!(event.attempt, self.attempt);
            events.push(event.event);
        }
        events
    }

    async fn authenticate(&mut self, bind: Option<&str>, notifications: bool) {
        let caps = format!(
            "{CAPS}{}",
            if notifications {
                " soju.im/bouncer-networks-notify"
            } else {
                ""
            }
        );
        self.send(&format!(
            "CAP * LS :{}",
            caps.replacen("sasl", "sasl=PLAIN", 1)
        ))
        .await;
        self.expect(&format!("CAP REQ :{caps}")).await;
        self.send(&format!("CAP * ACK :{caps}")).await;
        self.expect("AUTHENTICATE PLAIN").await;
        self.send("AUTHENTICATE +").await;
        self.expect(&format!(
            "AUTHENTICATE {}",
            BASE64.encode("\0account\0credential-secret")
        ))
        .await;
        // A fence also proves BIND, END, and connect commands have not preceded authentication.
        self.fence("before-auth").await;
        self.send(":soju 903 reader :Authenticated").await;
        if let Some(id) = bind {
            self.expect(&format!("BOUNCER BIND {id}")).await;
        }
        self.expect("CAP END").await;
        self.fence("before-welcome").await;
    }

    async fn welcome(&mut self, child: Option<&str>) {
        self.send(":soju 001 reader :Welcome").await;
        let bound = child
            .map(|id| format!(" BOUNCER_NETID={id}"))
            .unwrap_or_default();
        self.send(&format!(
            ":soju 005 reader CHATHISTORY=2 MSGREFTYPES=timestamp,msgid{bound} :supported"
        ))
        .await;
        self.send(":soju 376 reader :End of MOTD").await;
        if child.is_some() {
            self.expect("JOIN #configured").await;
        }
        self.fence("registered").await;
    }

    async fn finish(&mut self) -> (String, bool) {
        self.write.shutdown().await.unwrap();
        let end = self.task.take().unwrap().await.unwrap();
        let remaining = self.events();
        assert!(
            !remaining
                .iter()
                .any(|event| matches!(event, NetEvent::HistoryComplete { .. }))
        );
        assert!(matches!(
            self.events.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        end
    }
}

fn assert_no_credentials(events: &[NetEvent]) {
    let rendered = format!("{events:?}");
    for secret in [
        "credential-secret",
        "server-secret",
        &BASE64.encode("\0account\0credential-secret"),
    ] {
        assert!(
            !rendered.contains(secret),
            "protocol credentials reached an event"
        );
    }
}

#[tokio::test]
async fn control_two_children_nested_playback_markers_and_reconciliation() {
    let parent = account(101, None);
    let mut control = Peer::start(1, parent.clone()).await;
    control.authenticate(None, true).await;
    // Notifications may precede the role decision; only a complete snapshot is authoritative.
    control.send(":soju 001 reader :Welcome").await;
    control
        .send("BATCH +networks soju.im/bouncer-networks")
        .await;
    control.send("@batch=networks BOUNCER NETWORK net1 name=First;host=upstream-one.example;pass=server-secret").await;
    control
        .send("@batch=networks BOUNCER NETWORK net2 name=Second;host=upstream-two.example")
        .await;
    control.send("BATCH -networks").await;
    control.fence("early-snapshot").await;
    let early = control.events();
    assert!(!early.iter().any(|e| matches!(
        e,
        NetEvent::Role(_) | NetEvent::Network(_) | NetEvent::NetworkSnapshot(_)
    )));
    assert_no_credentials(&early);
    control
        .send(":soju 005 reader CHATHISTORY=2 MSGREFTYPES=timestamp,msgid :supported")
        .await;
    control.send(":soju 376 reader :End of MOTD").await;
    control.fence("control-ready").await;
    let events = control.events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, NetEvent::Role(ConnectionRole::Control)))
    );
    let snapshot = events
        .iter()
        .find_map(|event| match event {
            NetEvent::NetworkSnapshot(n) => Some(n.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(snapshot.len(), 2);
    let mut group = Group::default();
    let starts: Vec<_> = snapshot
        .into_iter()
        .flat_map(|n| group.apply(NetworkUpdate::Upsert(n)))
        .collect();
    assert_eq!(
        starts,
        [
            ChildAction::Start("net1".into()),
            ChildAction::Start("net2".into())
        ]
    );
    for event in events {
        if let NetEvent::Network(update) = event {
            assert!(group.apply(update).is_empty());
        }
    }
    let mut children = Vec::new();
    for (index, action) in starts.into_iter().enumerate() {
        let ChildAction::Start(id) = action else {
            panic!()
        };
        let mut registration = parent.clone();
        registration.options = SessionOptions {
            attempt: 201 + index as u64,
            bind_network: Some(id.clone()),
        };
        assert_eq!(
            (&registration.host, registration.port, registration.tls),
            (&parent.host, parent.port, parent.tls)
        );
        let mut peer = Peer::start(2 + index as u32, registration).await;
        peer.authenticate(Some(&id), false).await;
        peer.welcome(Some(&id)).await;
        assert!(
            peer.events()
                .iter()
                .any(|e| matches!(e, NetEvent::Role(ConnectionRole::Bound(bound)) if bound == &id))
        );
        assert!(group.registration_finished(&id).is_empty());
        children.push(peer);
    }

    let peer = &mut children[0];
    peer.sender
        .discover_history_targets(Some(crate::sync_state::Cursor {
            time: "2026-10-20T08:00:00Z".into(),
            msgid: None,
        }))
        .unwrap();
    let discovery: Message = peer.line().await.parse().unwrap();
    assert!(matches!(&discovery.command, Command::Raw(command,args)
        if command=="CHATHISTORY" && args[0]=="TARGETS" && args[2]=="timestamp=2026-10-20T07:59:55.000Z" && args[3]=="2"));
    peer.send("@draft/chathistory-end BATCH +targets draft/chathistory-targets")
        .await;
    peer.send("@batch=targets CHATHISTORY TARGETS bob 2026-10-20T09:00:00Z")
        .await;
    peer.send("@batch=targets CHATHISTORY TARGETS #parted 2026-10-20T09:30:00Z")
        .await;
    peer.send("BATCH -targets").await;
    // Discovery emits targets and does not send JOIN for a historical channel.
    peer.fence("missed-targets").await;
    let targets = peer.events();
    assert_eq!(
        targets
            .iter()
            .filter(|event| matches!(event, NetEvent::HistoryTarget { .. }))
            .count(),
        2
    );
    assert!(
        targets
            .iter()
            .any(|event| matches!(event,NetEvent::HistoryTarget{target,..} if target=="bob"))
    );
    for target in ["#configured", "bob"] {
        peer.sender
            .history(HistoryRequest {
                target: target.into(),
                reason: HistoryReason::Initial,
                count: 100,
                cursor: None,
            })
            .unwrap();
        peer.expect(&format!("CHATHISTORY LATEST {target} * 2"))
            .await;
    }
    peer.send("@draft/chathistory-end BATCH +channel chathistory #configured")
        .await;
    peer.send("@draft/chathistory-end BATCH +query chathistory bob")
        .await;
    peer.send("@batch=channel BATCH +nested vendor/nested")
        .await;
    peer.send("@batch=nested;time=2026-10-20T10:00:00Z;msgid=old1 :bob!u@h PRIVMSG #configured :\x01PING historical\x01").await;
    peer.send("@batch=nested :reader!u@h NICK historical-nick")
        .await;
    peer.send("@batch=nested;time=2026-10-20T10:00:00Z;msgid=old1b :bob!u@h NOTICE #configured :same timestamp after CTCP").await;
    peer.send(
        "@time=2026-10-20T10:01:00Z;msgid=live1 :bob!u@h PRIVMSG #configured :live during playback",
    )
    .await;
    peer.send(":bob!u@h PRIVMSG #configured :\x01PING live\x01")
        .await;
    peer.expect("NOTICE bob :\x01PING live\x01").await;
    peer.fence("live-responsive").await;
    let live = peer.events();
    assert_eq!(live.iter().filter(|e| matches!(e, NetEvent::Message(m) if matches!(&m.command, Command::PRIVMSG(_,body) if body == "live during playback"))).count(), 1);
    assert!(!live.iter().any(|e| matches!(
        e,
        NetEvent::HistoryComplete { .. }
            | NetEvent::HistoryMessage { .. }
            | NetEvent::NickChanged(_)
    )));
    peer.send("@batch=query;time=2026-10-20T09:00:00Z;msgid=old2 :bob!u@h PRIVMSG reader :private context").await;
    peer.send("@batch=channel BATCH -nested").await;
    peer.send("BATCH -query").await;
    peer.send("BATCH -channel").await;
    peer.send("MARKREAD #configured timestamp=2026-10-20T10:01:00Z")
        .await;
    peer.send("MARKREAD bob timestamp=2026-10-20T09:00:00Z")
        .await;
    peer.fence("history-and-markers").await;
    let history = peer.events();
    assert_eq!(
        history
            .iter()
            .filter(|e| matches!(
                e,
                NetEvent::HistoryComplete {
                    exhausted: true,
                    pending: false,
                    ..
                }
            ))
            .count(),
        2
    );
    assert_eq!(
        history
            .iter()
            .filter(|e| matches!(e, NetEvent::ReadMarker(_)))
            .count(),
        2
    );
    assert!(!history.iter().any(|e| matches!(
        e,
        NetEvent::HistoryMessage { .. } | NetEvent::NickChanged(_)
    )));
    assert!(!history.iter().any(|e| matches!(e, NetEvent::Message(m) if matches!(m.command, Command::PRIVMSG(..) | Command::NICK(..)))));

    let channel_messages = history
        .iter()
        .find_map(|event| match event {
            NetEvent::HistoryComplete {
                target, messages, ..
            } if target == "#configured" => Some(messages),
            _ => None,
        })
        .unwrap();
    assert_eq!(channel_messages.len(), 2);
    assert_eq!(
        crate::playback::tag(&channel_messages[0], "msgid"),
        Some("old1")
    );
    assert_eq!(
        crate::playback::tag(&channel_messages[1], "msgid"),
        Some("old1b")
    );
    let query_messages = history
        .iter()
        .find_map(|event| match event {
            NetEvent::HistoryComplete {
                target, messages, ..
            } if target == "bob" => Some(messages),
            _ => None,
        })
        .unwrap();
    assert_eq!(query_messages.len(), 1);
    assert_eq!(
        crate::playback::tag(&query_messages[0], "msgid"),
        Some("old2")
    );

    // An incomplete replacement snapshot may rename a child but cannot remove another child.
    control
        .send("BATCH +refresh soju.im/bouncer-networks")
        .await;
    control
        .send("@batch=refresh BOUNCER NETWORK net1 name=Renamed;state=disconnected")
        .await;
    control.fence("partial-snapshot").await;
    for event in control.events() {
        match event {
            NetEvent::Network(NetworkUpdate::Remove(_)) => {
                panic!("incomplete snapshot removed a child")
            }
            NetEvent::Network(update) => assert!(group.apply(update).is_empty()),
            _ => {}
        }
    }
    assert!(group.network("net2").is_some());
    assert_eq!(group.network("net1").unwrap().name(), "Renamed");
    control.send("BATCH -refresh").await;
    control.fence("complete-snapshot").await;
    let mut stops = Vec::new();
    for event in control.events() {
        if let NetEvent::Network(update) = event {
            stops.extend(group.apply(update));
        }
    }
    assert_eq!(stops, [ChildAction::Stop("net2".into())]);
    children[1].finish().await;
    assert!(group.registration_due("net2").is_empty());

    // Dropping the control session leaves the surviving child usable; a fresh
    // control attempt's repeated snapshot does not create a second child.
    control.finish().await;
    children[0].fence("parent-disconnected").await;
    let mut replacement = Peer::start(1, account(102, None)).await;
    replacement.authenticate(None, true).await;
    replacement.welcome(None).await;
    replacement
        .send("BATCH +replacement soju.im/bouncer-networks")
        .await;
    replacement
        .send("@batch=replacement BOUNCER NETWORK net1 name=Renamed")
        .await;
    replacement.send("BATCH -replacement").await;
    replacement.fence("replacement-ready").await;
    for event in replacement.events() {
        if let NetEvent::Network(update) = event {
            assert!(group.apply(update).is_empty());
        }
    }
    assert_eq!(group.disconnect(), [ChildAction::Stop("net1".into())]);
    children[0].finish().await;
    replacement.finish().await;
}

#[tokio::test]
async fn capability_removal_discards_late_requested_history_and_sanitizes_protocol_errors() {
    let mut peer = Peer::start(9, account(301, None)).await;
    peer.authenticate(None, true).await;
    peer.welcome(None).await;
    peer.events();
    peer.sender
        .history(HistoryRequest {
            target: "bob".into(),
            reason: HistoryReason::Initial,
            count: 2,
            cursor: None,
        })
        .unwrap();
    peer.expect("CHATHISTORY LATEST bob * 2").await;
    peer.sender
        .history(HistoryRequest {
            target: "alice".into(),
            reason: HistoryReason::Initial,
            count: 2,
            cursor: None,
        })
        .unwrap();
    peer.expect("CHATHISTORY LATEST alice * 2").await;
    peer.send("BATCH +started chathistory alice").await;
    peer.send("@batch=started BATCH +canceledNested vendor/nested")
        .await;
    peer.fence("batch-before-removal").await;
    peer.events();
    peer.send("CAP * DEL :draft/chathistory").await;
    peer.send("@batch=canceledNested;time=2026-10-20T10:00:00Z :alice!u@h PRIVMSG reader :canceled nested history").await;
    peer.send("@batch=started BATCH -canceledNested").await;
    peer.send("BATCH -started").await;
    peer.send("BATCH +late chathistory bob").await;
    peer.send(
        "@batch=late;time=2026-10-20T10:00:00Z :bob!u@h PRIVMSG reader :late canceled history",
    )
    .await;
    peer.send("BATCH -late").await;
    peer.send("AUTHENTICATE credential-secret").await;
    peer.send("AUTHENTICATE credential-secret extra").await;
    peer.send("FAIL BOUNCER INVALID_ATTRIBUTE ADDNETWORK * pass=server-secret :bad server-secret")
        .await;
    peer.send(":soju 461 reader BOUNCER :bad pass=server-secret")
        .await;
    peer.fence("canceled-and-sanitized").await;
    let events = peer.events();
    assert_no_credentials(&events);
    assert!(!events.iter().any(|e| matches!(
        e,
        NetEvent::HistoryComplete { .. } | NetEvent::HistoryMessage { .. }
    )));
    peer.sender
        .history(HistoryRequest {
            target: "bob".into(),
            reason: HistoryReason::Initial,
            count: 2,
            cursor: None,
        })
        .unwrap();
    // The typed request is rejected by capability state without any wire query.
    let (id, event) = tokio::time::timeout(Duration::from_secs(2), peer.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(id, 9);
    assert_eq!(event.attempt, 301);
    assert!(
        matches!(event.event, NetEvent::HistoryNotice { target:Some(target),.. } if target=="bob")
    );
    peer.fence("unsupported-history").await;
    peer.finish().await;
}

#[tokio::test(start_paused = true)]
async fn stalled_authentication_and_history_are_bounded_and_do_not_leak_secrets() {
    let mut stalled = Peer::start(10, account(401, Some("net1"))).await;
    stalled
        .send("CAP * LS :sasl=PLAIN soju.im/bouncer-networks")
        .await;
    stalled
        .expect("CAP REQ :sasl soju.im/bouncer-networks")
        .await;
    stalled
        .send("CAP * ACK :sasl soju.im/bouncer-networks")
        .await;
    stalled.expect("AUTHENTICATE PLAIN").await;
    tokio::time::advance(Duration::from_secs(31)).await;
    let (reason, _) = stalled.task.take().unwrap().await.unwrap();
    assert!(reason.contains("timed out"));
    assert!(!reason.contains("credential-secret"));
    assert_no_credentials(&stalled.events());

    let mut peer = Peer::start(11, account(402, None)).await;
    peer.authenticate(None, true).await;
    peer.welcome(None).await;
    peer.events();
    peer.sender
        .history(HistoryRequest {
            target: "bob".into(),
            reason: HistoryReason::Initial,
            count: 2,
            cursor: None,
        })
        .unwrap();
    peer.expect("CHATHISTORY LATEST bob * 2").await;
    tokio::time::advance(Duration::from_secs(31)).await;
    peer.fence("after-timeout").await;
    assert!(peer.events().iter().any(|event| matches!(event, NetEvent::HistoryNotice{target:Some(target),text} if target=="bob" && text.contains("timed out"))));
    peer.send("BATCH +timedout chathistory bob").await;
    peer.send(
        "@batch=timedout;time=2026-10-20T10:00:00Z :bob!u@h PRIVMSG reader :late timeout history",
    )
    .await;
    peer.send("BATCH -timedout").await;
    peer.fence("late-timeout").await;
    assert!(!peer.events().iter().any(|event| matches!(
        event,
        NetEvent::HistoryMessage { .. } | NetEvent::HistoryComplete { .. }
    )));
    peer.send("BOUNCER NETWORK bad name=Hidden pass=server-secret")
        .await;
    let (reason, _) = peer.task.take().unwrap().await.unwrap();
    assert_eq!(reason, "Invalid bouncer network update.");
    assert_no_credentials(&peer.events());
}
