#[cfg(test)]
mod tests {
    use super::*;
    fn request(target: &str) -> HistoryRequest {
        HistoryRequest {
            target: target.into(),
            reason: HistoryReason::Initial,
            count: 100,
            cursor: None,
        }
    }
    #[test]
    fn clamps_pages_and_serializes_targets_with_bounded_concurrency() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport {
            limit: Some(50),
            timestamp_refs: true,
            msgid_refs: true,
        });
        let a = q.request(request("#test"), now).unwrap();
        assert!(
            matches!(&a[0], HistoryAction::Send(m) if m.to_string() == "CHATHISTORY LATEST #test * 50\r\n")
        );
        assert!(q.request(request("#TEST"), now).is_err());
        q.request(request("bob"), now).unwrap();
        assert!(q.request(request("alice"), now).unwrap().is_empty());
        let a = q.tick(now + Duration::from_secs(31));
        assert!(a.iter().any(|a| matches!(a, HistoryAction::Notice { .. })));
        assert!(
            a.iter()
                .any(|a| matches!(a, HistoryAction::Send(m) if m.to_string().contains("alice")))
        );
        assert!(q.request(request("#test"), now).is_err()); // late batch cannot finish a newer request
    }
    #[test]
    fn id_overlap_and_identical_live_messages() {
        let mut d = Deduper::new(&[]);
        let m: Message = "@time=2026-10-20T10:00:00Z :b!u@h PRIVMSG me :hi"
            .parse()
            .unwrap();
        assert!(d.accept(&m, false));
        assert!(d.accept(&m, false));
        d.begin_history();
        assert!(!d.accept(&m, true));
        assert!(!d.accept(&m, true));
        assert!(d.accept(&m, true));
        d.end_history();
        let m: Message = "@msgid=42 :b!u@h PRIVMSG me :hi".parse().unwrap();
        assert!(d.accept(&m, false));
        assert!(!d.accept(&m, true));
        assert_eq!(d.recent_ids(256), ["42"]);
    }
    #[test]
    fn catchup_overlaps_timestamp_and_older_uses_id() {
        let mut q = HistoryQueue::new(HistorySupport {
            timestamp_refs: true,
            msgid_refs: false,
            limit: None,
        });
        let mut r = request("bob");
        r.reason = HistoryReason::CatchUp;
        r.cursor = Some(Cursor {
            time: "2026-10-20T10:00:00Z".into(),
            msgid: Some("42".into()),
        });
        let a = q.request(r, Instant::now()).unwrap();
        assert!(
            matches!(&a[0],HistoryAction::Send(m) if m.to_string().contains("AFTER bob timestamp=2026-10-20T09:59:55"))
        );
    }
    fn feed(
        q: &mut HistoryQueue,
        tracker: &mut super::super::BatchTracker,
        line: &str,
        now: Instant,
    ) -> Vec<HistoryAction> {
        q.handle(&tracker.ingest(line.parse().unwrap()).unwrap(), now)
            .unwrap()
    }
    fn page(
        q: &mut HistoryQueue,
        tracker: &mut super::super::BatchTracker,
        id: &str,
        target: &str,
        times: &[&str],
        now: Instant,
    ) -> Vec<HistoryAction> {
        feed(
            q,
            tracker,
            &format!("BATCH +{id} chathistory {target}"),
            now,
        );
        for time in times {
            feed(
                q,
                tracker,
                &format!("@batch={id};time={time} :b!u@h PRIVMSG {target} :old"),
                now,
            );
        }
        feed(q, tracker, &format!("BATCH -{id}"), now)
    }
    #[test]
    fn catchup_uses_overlap_once_then_advances_without_subtracting_again() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport {
            limit: Some(2),
            timestamp_refs: true,
            msgid_refs: false,
        });
        let mut r = request("bob");
        r.reason = HistoryReason::CatchUp;
        r.cursor = Some(Cursor {
            time: "2026-10-20T10:00:00.000Z".into(),
            msgid: None,
        });
        let a = q.request(r, now).unwrap();
        assert!(
            matches!(&a[0],HistoryAction::Send(m) if m.to_string()=="CHATHISTORY AFTER bob timestamp=2026-10-20T09:59:55.000Z 2\r\n")
        );
        let a = page(
            &mut q,
            &mut super::super::BatchTracker::default(),
            "a",
            "bob",
            &["2026-10-20T10:00:01.000Z", "2026-10-20T10:00:02.000Z"],
            now,
        );
        assert!(a.iter().any(|a|matches!(a,HistoryAction::Send(m) if m.to_string()=="CHATHISTORY AFTER bob timestamp=2026-10-20T10:00:02.000Z 2\r\n")));
    }
    #[test]
    fn prefers_supported_ids_for_catchup_and_before() {
        let now = Instant::now();
        for reason in [HistoryReason::CatchUp, HistoryReason::Older] {
            let mut q = HistoryQueue::new(HistorySupport::default());
            let mut r = request("bob");
            r.reason = reason;
            r.cursor = Some(Cursor {
                time: "2026-10-20T10:00:00Z".into(),
                msgid: Some("42".into()),
            });
            let a = q.request(r, now).unwrap();
            assert!(
                matches!(&a[0],HistoryAction::Send(m) if m.to_string().contains("bob msgid=42"))
            );
        }
    }
    #[test]
    fn full_initial_and_older_pages_do_not_claim_exhausted() {
        let now = Instant::now();
        for reason in [HistoryReason::Initial, HistoryReason::Older] {
            let mut q = HistoryQueue::new(HistorySupport {
                limit: Some(2),
                ..HistorySupport::default()
            });
            let mut r = request("bob");
            r.reason = reason;
            q.request(r, now).unwrap();
            let a = page(
                &mut q,
                &mut super::super::BatchTracker::default(),
                "a",
                "bob",
                &["2026-10-20T10:00:01Z", "2026-10-20T10:00:02Z"],
                now,
            );
            assert!(a.iter().any(|a| matches!(
                a,
                HistoryAction::Complete {
                    exhausted: false,
                    ..
                }
            )));
        }
    }
    #[test]
    fn no_progress_does_not_report_history_complete() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport {
            limit: Some(1),
            timestamp_refs: true,
            msgid_refs: false,
        });
        let mut r = request("bob");
        r.reason = HistoryReason::CatchUp;
        r.cursor = Some(Cursor {
            time: "2026-10-20T10:00:00Z".into(),
            msgid: None,
        });
        q.request(r, now).unwrap();
        let mut t = super::super::BatchTracker::default();
        let a = page(&mut q, &mut t, "a", "bob", &["2026-10-20T10:00:00Z"], now);
        assert!(a.iter().any(|a| matches!(a, HistoryAction::Send(_))));
        let a = page(
            &mut q,
            &mut t,
            "b",
            "bob",
            &["2026-10-20T10:00:00.000Z"],
            now,
        );
        assert!(a.iter().any(|a| matches!(
            a,
            HistoryAction::Complete {
                exhausted: false,
                ..
            }
        )));
        assert!(a.iter().any(|a| matches!(a, HistoryAction::Notice { .. })));
        assert!(!a.iter().any(|a| matches!(a, HistoryAction::Send(_))));
    }
    #[test]
    fn target_discovery_moves_newest_bound_backwards() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport {
            limit: Some(2),
            ..HistorySupport::default()
        });
        let a = q
            .targets(
                Some(Cursor {
                    time: "2026-10-20T10:00:00Z".into(),
                    msgid: None,
                }),
                now,
            )
            .unwrap();
        let HistoryAction::Send(m) = &a[0] else {
            panic!()
        };
        let Command::Raw(_, args) = &m.command else {
            panic!()
        };
        assert_eq!(args[2], "timestamp=2026-10-20T09:59:55.000Z");
        let mut t = super::super::BatchTracker::default();
        feed(&mut q, &mut t, "BATCH +a draft/chathistory-targets", now);
        feed(
            &mut q,
            &mut t,
            "@batch=a CHATHISTORY TARGETS bob 2026-10-20T10:02:00Z",
            now,
        );
        feed(
            &mut q,
            &mut t,
            "@batch=a CHATHISTORY TARGETS alice 2026-10-20T10:01:00Z",
            now,
        );
        let a = feed(&mut q, &mut t, "BATCH -a", now);
        assert!(a.iter().any(|a|matches!(a,HistoryAction::Send(m) if m.to_string()=="CHATHISTORY TARGETS timestamp=2026-10-20T10:01:00Z timestamp=2026-10-20T09:59:55.000Z 2\r\n")));
    }
    #[test]
    fn only_top_level_matching_batches_bind_requests() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport::default());
        q.request(request("bob"), now).unwrap();
        let mut t = super::super::BatchTracker::default();
        feed(&mut q, &mut t, "BATCH +outer unknown", now);
        feed(
            &mut q,
            &mut t,
            "@batch=outer BATCH +nested chathistory bob",
            now,
        );
        assert!(!q.handles("nested"));
    }
    #[test]
    fn fail_only_cancels_the_named_conversation() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport::default());
        q.request(request("bob"), now).unwrap();
        q.request(request("alice"), now).unwrap();
        let mut t = super::super::BatchTracker::default();
        let a = feed(
            &mut q,
            &mut t,
            "FAIL CHATHISTORY INVALID_TARGET LATEST bob :No history",
            now,
        );
        assert_eq!(
            a.iter()
                .filter(|a| matches!(a, HistoryAction::Notice { .. }))
                .count(),
            1
        );
        let a = page(&mut q, &mut t, "a", "alice", &[], now);
        assert!(
            a.iter()
                .any(|a| matches!(a,HistoryAction::Complete{target,..} if target=="alice"))
        );
    }
    #[test]
    fn canceled_request_cannot_be_replaced_and_hijacked_by_late_batches() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport::default());
        q.request(request("bob"), now).unwrap();
        q.cancel();
        assert!(q.request(request("bob"), now).is_err());
    }
    #[test]
    fn four_megabyte_buffer_bound_is_shared_between_requests() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport::default());
        q.request(request("bob"), now).unwrap();
        q.request(request("alice"), now).unwrap();
        let mut t = super::super::BatchTracker::default();
        feed(&mut q, &mut t, "BATCH +a chathistory bob", now);
        feed(&mut q, &mut t, "BATCH +b chathistory alice", now);
        let body = "x".repeat(2 * 1024 * 1024 + 1);
        feed(
            &mut q,
            &mut t,
            &format!("@batch=a :b!u@h PRIVMSG bob :{body}"),
            now,
        );
        let e = t
            .ingest(
                format!("@batch=b :b!u@h PRIVMSG alice :{body}")
                    .parse()
                    .unwrap(),
            )
            .unwrap();
        assert!(q.handle(&e, now).is_err());
    }
    #[test]
    fn no_id_occurrence_accounting_stays_bounded_and_invalid_times_are_not_deduped() {
        let mut d = Deduper::new(&[]);
        d.begin_history();
        for n in 0..5000 {
            let m: Message = format!("@time=2026-10-20T10:00:00Z :b!u@h PRIVMSG me :{n}")
                .parse()
                .unwrap();
            assert!(d.accept(&m, true));
        }
        assert!(d.seen.len() <= 4096);
        d.end_history();
        let m: Message = "@time=invalid :b!u@h PRIVMSG me :hi".parse().unwrap();
        assert!(d.accept(&m, false));
        d.begin_history();
        assert!(d.accept(&m, true));
    }

    #[test]
    fn stale_failure_for_other_target_does_not_cancel_active_history() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport::default());
        q.request(request("alice"), now).unwrap();
        let mut t = super::super::BatchTracker::default();
        assert!(
            feed(
                &mut q,
                &mut t,
                "FAIL CHATHISTORY INVALID_TARGET LATEST bob :No history",
                now
            )
            .is_empty()
        );
        assert!(q.has_pending("alice"));
    }
    #[test]
    fn repeated_id_selector_cycle_stops_without_claiming_exhaustion() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport {
            limit: Some(1),
            ..HistorySupport::default()
        });
        let mut r = request("bob");
        r.reason = HistoryReason::CatchUp;
        r.cursor = Some(Cursor {
            time: "2026-10-20T10:00:00Z".into(),
            msgid: Some("first".into()),
        });
        q.request(r, now).unwrap();
        let mut t = super::super::BatchTracker::default();
        for (batch, id) in [("a", "second"), ("b", "first")] {
            feed(
                &mut q,
                &mut t,
                &format!("BATCH +{batch} chathistory bob"),
                now,
            );
            feed(
                &mut q,
                &mut t,
                &format!(
                    "@batch={batch};time=2026-10-20T10:00:00Z;msgid={id} :b!u@h PRIVMSG bob :hi"
                ),
                now,
            );
            let a = feed(&mut q, &mut t, &format!("BATCH -{batch}"), now);
            if batch == "b" {
                assert!(!a.iter().any(|a| matches!(a, HistoryAction::Send(_))));
                assert!(a.iter().any(|a| matches!(
                    a,
                    HistoryAction::Complete {
                        pending: false,
                        exhausted: false,
                        ..
                    }
                )));
                assert!(a.iter().any(|a| matches!(a, HistoryAction::Notice { .. })));
            }
        }
    }

    #[test]
    fn duplicate_target_rows_still_consume_the_discovery_budget() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport {
            limit: Some(1),
            ..HistorySupport::default()
        });
        q.targets(None, now).unwrap();
        let mut t = super::super::BatchTracker::default();
        for n in 0..100 {
            let time = chrono::DateTime::parse_from_rfc3339("2026-10-20T10:00:00Z").unwrap()
                - chrono::Duration::seconds(n);
            let time = time.to_utc().to_rfc3339();
            feed(
                &mut q,
                &mut t,
                &format!("BATCH +a{n} draft/chathistory-targets"),
                now,
            );
            feed(
                &mut q,
                &mut t,
                &format!("@batch=a{n} CHATHISTORY TARGETS bob {time}"),
                now,
            );
            let a = feed(&mut q, &mut t, &format!("BATCH -a{n}"), now);
            if n == 99 {
                assert!(!a.iter().any(|a| matches!(a, HistoryAction::Send(_))));
                assert!(a.iter().any(|a| matches!(a, HistoryAction::Notice { .. })));
            }
        }
    }
    #[test]
    fn automatic_catchup_stops_at_a_thousand_messages_without_claiming_exhaustion() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport::default());
        let mut r = request("bob");
        r.reason = HistoryReason::CatchUp;
        r.count = 1000;
        r.cursor = Some(Cursor {
            time: "2026-10-20T10:00:00Z".into(),
            msgid: Some("old".into()),
        });
        q.request(r, now).unwrap();
        let mut t = super::super::BatchTracker::default();
        feed(&mut q, &mut t, "BATCH +a chathistory bob", now);
        for n in 0..1000 {
            feed(
                &mut q,
                &mut t,
                &format!("@batch=a;time=2026-10-20T10:00:00Z;msgid=id{n} :b!u@h PRIVMSG bob :hi"),
                now,
            );
        }
        let a = feed(&mut q, &mut t, "BATCH -a", now);
        assert!(a.iter().any(|a|matches!(a,HistoryAction::Complete{messages,pending:false,exhausted:false,..} if messages.len()==1000)));
        assert!(a.iter().any(|a| matches!(a, HistoryAction::Notice { .. })));
        assert!(!q.has_pending("bob"));
    }
    #[test]
    fn rejects_a_thousand_and_first_message_but_releases_finished_buffer_bytes() {
        let now = Instant::now();
        let mut q = HistoryQueue::new(HistorySupport::default());
        q.request(request("bob"), now).unwrap();
        let mut t = super::super::BatchTracker::default();
        feed(&mut q, &mut t, "BATCH +a chathistory bob", now);
        for _ in 0..1000 {
            feed(&mut q, &mut t, "@batch=a :b!u@h PRIVMSG bob :hi", now);
        }
        let e = t
            .ingest("@batch=a :b!u@h PRIVMSG bob :too many".parse().unwrap())
            .unwrap();
        assert!(q.handle(&e, now).is_err());
        feed(&mut q, &mut t, "BATCH -a", now);
        q.request(request("alice"), now).unwrap();
        feed(&mut q, &mut t, "BATCH +b chathistory alice", now);
        feed(
            &mut q,
            &mut t,
            &format!(
                "@batch=b :b!u@h PRIVMSG alice :{}",
                "x".repeat(4 * 1024 * 1024 - 128)
            ),
            now,
        );
    }
    #[test]
    fn combined_target_and_reference_must_fit_one_irc_command() {
        let mut q = HistoryQueue::new(HistorySupport::default());
        let target = "a".repeat(300);
        let actions = q
            .request(
                HistoryRequest {
                    target: target.clone(),
                    reason: HistoryReason::Older,
                    count: 100,
                    cursor: Some(Cursor {
                        time: "2026-10-20T10:00:00Z".into(),
                        msgid: Some("b".repeat(300)),
                    }),
                },
                Instant::now(),
            )
            .unwrap();
        assert!(!actions.iter().any(|a| matches!(a, HistoryAction::Send(_))));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a,HistoryAction::Notice{target:Some(t),..} if t==&target))
        );
        assert!(!q.has_pending(&target));
    }
    #[test]
    fn opaque_message_ids_preserve_internal_colons_and_long_ids() {
        let id = format!("origin:{}", "x".repeat(450));
        let message: Message =
            format!("@time=2026-10-20T10:00:00Z;msgid={id} :b!u@h PRIVMSG me :hi")
                .parse()
                .unwrap();
        let mut deduper = Deduper::new(&[]);
        assert!(deduper.accept(&message, false));
        assert!(!deduper.accept(&message, true));
        assert_eq!(
            cursor(&message).unwrap().msgid.as_deref(),
            Some(id.as_str())
        );
        let mut q = HistoryQueue::new(HistorySupport::default());
        let actions = q
            .request(
                HistoryRequest {
                    target: "bob".into(),
                    reason: HistoryReason::Older,
                    count: 1,
                    cursor: Some(Cursor {
                        time: "2026-10-20T10:00:00Z".into(),
                        msgid: Some("origin:42".into()),
                    }),
                },
                Instant::now(),
            )
            .unwrap();
        assert!(actions.iter().any(|a|matches!(a,HistoryAction::Send(m) if m.to_string()=="CHATHISTORY BEFORE bob msgid=origin:42 1\r\n")));
    }
    #[test]
    fn catchup_budget_is_per_conversation_for_the_whole_connection_attempt() {
        let now = Instant::now();
        let mut queue = HistoryQueue::new(HistorySupport::default());
        let mut tracker = super::super::BatchTracker::default();
        let mut catchup = HistoryRequest {
            target: "bob".into(),
            reason: HistoryReason::CatchUp,
            count: 1000,
            cursor: Some(Cursor {
                time: "2026-10-20T10:00:00Z".into(),
                msgid: Some("before".into()),
            }),
        };
        queue.request(catchup.clone(), now).unwrap();
        feed(
            &mut queue,
            &mut tracker,
            "BATCH +first chathistory bob",
            now,
        );
        for index in 0..600 {
            feed(
                &mut queue,
                &mut tracker,
                &format!(
                    "@batch=first;time=2026-10-20T10:00:01Z;msgid=first{index} :bob!u@h PRIVMSG me :overlap or new"
                ),
                now,
            );
        }
        let completed = feed(&mut queue, &mut tracker, "BATCH -first", now);
        assert!(
            completed
                .iter()
                .any(|action| matches!(action, HistoryAction::Complete { pending: false, .. }))
        );
        // Reopening a case-equivalent conversation, including after cancel, cannot
        // replenish the automatic budget. Each returned overlap occurrence counts.
        queue.cancel();
        catchup.target = "BOB".into();
        let actions = queue.request(catchup.clone(), now).unwrap();
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, HistoryAction::Send(message)
            if message.to_string() == "CHATHISTORY AFTER BOB msgid=before 400\r\n"))
        );
        feed(
            &mut queue,
            &mut tracker,
            "BATCH +second chathistory bob",
            now,
        );
        for index in 0..400 {
            feed(
                &mut queue,
                &mut tracker,
                &format!(
                    "@batch=second;time=2026-10-20T10:00:01Z;msgid=second{index} :bob!u@h PRIVMSG me :overlap or new"
                ),
                now,
            );
        }
        let completed = feed(&mut queue, &mut tracker, "BATCH -second", now);
        assert!(completed.iter().any(|action| matches!(
            action,
            HistoryAction::Complete {
                pending: false,
                exhausted: false,
                ..
            }
        )));
        assert!(
            completed
                .iter()
                .any(|action| matches!(action, HistoryAction::Notice { .. }))
        );
        assert!(
            !completed
                .iter()
                .any(|action| matches!(action, HistoryAction::Send(_)))
        );
        let error = queue.request(catchup.clone(), now).unwrap_err();
        assert!(error.contains("/history"));
        catchup.reason = HistoryReason::Older;
        let actions = queue.request(catchup, now).unwrap();
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, HistoryAction::Send(message)
            if message.to_string() == "CHATHISTORY BEFORE BOB msgid=before 1000\r\n"))
        );
        let mut other = request("alice");
        other.count = 1000;
        let actions = queue.request(other, now).unwrap();
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, HistoryAction::Send(message)
            if message.to_string() == "CHATHISTORY LATEST alice * 1000\r\n"))
        );
    }
}

use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{SecondsFormat, Utc};
use irc_proto::{Command, Message};
use tokio::time::{Duration, Instant};

use super::{BatchBoundary, BatchEvent, tag};
use crate::route::irc_lower;
use crate::sync_state::{Cursor, timestamp};

const HISTORY_LIMIT: usize = 1000;
const TARGET_LIMIT: usize = 100;
const BUFFER_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryReason {
    Initial,
    CatchUp,
    Older,
}

#[derive(Debug, Clone)]
pub struct HistorySupport {
    pub limit: Option<usize>,
    pub timestamp_refs: bool,
    pub msgid_refs: bool,
}

impl Default for HistorySupport {
    fn default() -> Self {
        // CHATHISTORY specifies both reference types when MSGREFTYPES is absent.
        Self {
            limit: None,
            timestamp_refs: true,
            msgid_refs: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HistoryRequest {
    pub target: String,
    pub reason: HistoryReason,
    pub count: usize,
    pub cursor: Option<Cursor>,
}

#[derive(Debug)]
pub enum HistoryAction {
    Send(Message),
    Target {
        target: String,
        latest: String,
    },
    Complete {
        target: String,
        reason: HistoryReason,
        messages: Vec<Message>,
        exhausted: bool,
        /// A further page of this automatic catch-up is still pending.
        pending: bool,
    },
    Notice {
        target: Option<String>,
        text: String,
    },
}

struct Pending {
    request: HistoryRequest,
    total: usize,
    overlap: bool,
    selectors: HashSet<String>,
}

struct Active {
    pending: Pending,
    deadline: Instant,
    messages: Vec<Message>,
    bytes: usize,
    batch: Option<String>,
    end: bool,
    page: usize,
    selector: String,
}

pub struct HistoryQueue {
    pub support: HistorySupport,
    waiting: VecDeque<Pending>,
    active: HashMap<String, Active>,
    // Unlabelled late replies cannot be distinguished from replies to a retry.
    // Keep these tombstones for the lifetime of the connection attempt.
    timed_out: HashSet<String>,
    targets_range: Option<(String, String)>,
    targets_seen: HashSet<String>,
    targets_received: usize,
    discarded: HashSet<String>,
    // Completed catch-up pages consume the connection attempt's conversation
    // budget even when a tab is closed/reopened or session work is canceled.
    catch_up_received: HashMap<String, usize>,
}

// Message IDs are opaque tag values, independent of bouncer IDs or targets.
fn valid_msgid(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with(':')
        && !id
            .bytes()
            .any(|byte| matches!(byte, b' ' | b'\r' | b'\n' | 0))
}

fn raw(args: Vec<String>) -> Message {
    Command::Raw("CHATHISTORY".into(), args).into()
}

fn verb(request: &HistoryRequest) -> &'static str {
    match (request.reason, request.cursor.is_some()) {
        (HistoryReason::Older, true) => "BEFORE",
        (HistoryReason::CatchUp, true) => "AFTER",
        _ => "LATEST",
    }
}

impl HistoryQueue {
    pub fn new(support: HistorySupport) -> Self {
        Self {
            support,
            waiting: VecDeque::new(),
            active: HashMap::new(),
            timed_out: HashSet::new(),
            targets_range: None,
            targets_seen: HashSet::new(),
            targets_received: 0,
            discarded: HashSet::new(),
            catch_up_received: HashMap::new(),
        }
    }

    pub fn has_pending(&self, target: &str) -> bool {
        let key = irc_lower(target);
        self.active.contains_key(&key)
            || self
                .waiting
                .iter()
                .any(|p| irc_lower(&p.request.target) == key)
    }

    pub fn request(
        &mut self,
        request: HistoryRequest,
        now: Instant,
    ) -> Result<Vec<HistoryAction>, String> {
        if request.count == 0
            || request.count > HISTORY_LIMIT
            || request.target == "*"
            || !crate::bouncer::valid_id(&request.target)
        {
            return Err("Invalid history request.".into());
        }
        let key = irc_lower(&request.target);
        if self.has_pending(&request.target) {
            return Err("History is already pending for this conversation.".into());
        }
        if self.timed_out.contains(&key) {
            return Err(
                "History was canceled or timed out; reconnect before retrying this conversation."
                    .into(),
            );
        }
        if self.waiting.len() >= TARGET_LIMIT {
            return Err("History request queue is full.".into());
        }
        let total = if request.reason == HistoryReason::CatchUp {
            let received = self.catch_up_received.get(&key).copied().unwrap_or(0);
            if received >= HISTORY_LIMIT {
                return Err("Automatic catch-up reached 1000 messages for this conversation; use /history for older context.".into());
            }
            received
        } else {
            0
        };
        self.selector(&request, true)?;
        self.waiting.push_back(Pending {
            request,
            total,
            overlap: true,
            selectors: HashSet::new(),
        });
        Ok(self.drain(now))
    }

    fn selector(&self, request: &HistoryRequest, overlap: bool) -> Result<String, String> {
        let Some(cursor) = &request.cursor else {
            return Ok("*".into());
        };
        if self.support.msgid_refs {
            if let Some(id) = &cursor.msgid {
                if valid_msgid(id) {
                    return Ok(format!("msgid={id}"));
                }
            }
        }
        if self.support.timestamp_refs {
            let mut time = timestamp(&cursor.time)?;
            if request.reason == HistoryReason::CatchUp && overlap {
                time -= chrono::Duration::seconds(5);
            }
            return Ok(format!(
                "timestamp={}",
                time.to_utc().to_rfc3339_opts(SecondsFormat::Millis, true)
            ));
        }
        Err("The server offers no usable history reference type.".into())
    }

    fn drain(&mut self, now: Instant) -> Vec<HistoryAction> {
        let mut actions = Vec::new();
        while self.active.len() < 2 {
            let Some(mut pending) = self.waiting.pop_front() else {
                break;
            };
            let request = &pending.request;
            let mut page = request
                .count
                .min(
                    self.support
                        .limit
                        .filter(|n| *n > 0)
                        .unwrap_or(HISTORY_LIMIT),
                )
                .min(HISTORY_LIMIT - pending.total);
            let (args, selector) = if request.target == "*" {
                let Some((newest, oldest)) = &self.targets_range else {
                    continue;
                };
                page = page.min(TARGET_LIMIT - self.targets_received);
                (
                    vec![
                        "TARGETS".into(),
                        format!("timestamp={newest}"),
                        format!("timestamp={oldest}"),
                        page.to_string(),
                    ],
                    String::new(),
                )
            } else {
                let selector = match self.selector(request, pending.overlap) {
                    Ok(selector) => selector,
                    Err(text) => {
                        actions.push(HistoryAction::Notice {
                            target: Some(request.target.clone()),
                            text,
                        });
                        continue;
                    }
                };
                (
                    vec![
                        verb(request).into(),
                        request.target.clone(),
                        selector.clone(),
                        page.to_string(),
                    ],
                    selector,
                )
            };
            if page == 0 {
                continue;
            }
            let message = raw(args);
            if message.to_string().len() > 512 {
                actions.push(HistoryAction::Notice {
                    target: Some(request.target.clone()),
                    text: "The history target and reference exceed the IRC command limit.".into(),
                });
                continue;
            }
            actions.push(HistoryAction::Send(message));
            let key = irc_lower(&request.target);
            pending.selectors.insert(selector.clone());
            self.active.insert(
                key,
                Active {
                    pending,
                    deadline: now + Duration::from_secs(30),
                    messages: Vec::new(),
                    bytes: 0,
                    batch: None,
                    end: false,
                    page,
                    selector,
                },
            );
        }
        actions
    }

    pub fn targets(
        &mut self,
        since: Option<Cursor>,
        now: Instant,
    ) -> Result<Vec<HistoryAction>, String> {
        if self.targets_range.is_some() {
            return Ok(Vec::new());
        }
        if self.timed_out.contains("*") {
            return Err(
                "History discovery was canceled or timed out; reconnect before retrying.".into(),
            );
        }
        if !self.support.timestamp_refs {
            return Err("History target discovery requires timestamp references.".into());
        }
        let oldest = since
            .and_then(|c| timestamp(&c.time).ok())
            .map(|t| {
                (t - chrono::Duration::seconds(5))
                    .to_utc()
                    .to_rfc3339_opts(SecondsFormat::Millis, true)
            })
            .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into());
        let newest = (Utc::now() + chrono::Duration::seconds(5))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        // BETWEEN semantics start at the first selector: prioritize recent targets.
        self.targets_range = Some((newest, oldest));
        self.waiting.push_back(Pending {
            request: HistoryRequest {
                target: "*".into(),
                reason: HistoryReason::Initial,
                count: TARGET_LIMIT,
                cursor: None,
            },
            total: 0,
            overlap: false,
            selectors: HashSet::new(),
        });
        Ok(self.drain(now))
    }

    /// Requested batches, including canceled replies that must remain suppressed.
    pub fn handles(&self, batch: &str) -> bool {
        self.discarded.contains(batch)
            || self
                .active
                .values()
                .any(|a| a.batch.as_deref() == Some(batch))
    }

    pub fn handle(
        &mut self,
        event: &BatchEvent,
        now: Instant,
    ) -> Result<Vec<HistoryAction>, String> {
        let mut actions = Vec::new();
        match event {
            BatchEvent::Boundary(BatchBoundary::Start(meta))
                if meta.parent.is_none()
                    && (meta.kind == "chathistory" || meta.kind == "draft/chathistory-targets") =>
            {
                let key = if meta.kind == "draft/chathistory-targets" {
                    "*".into()
                } else {
                    irc_lower(meta.target.as_deref().unwrap_or(""))
                };
                if self.timed_out.contains(&key) {
                    self.discarded.insert(meta.id.clone());
                } else if let Some(active) = self.active.get_mut(&key) {
                    if active.batch.is_none() {
                        active.batch = Some(meta.id.clone());
                        active.end = meta.exhausted;
                    }
                }
            }
            BatchEvent::Message {
                message,
                history: true,
                batch: Some(batch),
                ..
            } => {
                let buffered: usize = self.active.values().map(|a| a.bytes).sum();
                if let Some(active) = self
                    .active
                    .values_mut()
                    .find(|a| a.batch.as_ref() == Some(batch))
                {
                    let size = message.to_string().len();
                    if active.messages.len() + active.pending.total >= HISTORY_LIMIT
                        || buffered + size > BUFFER_LIMIT
                        || (active.pending.request.target == "*"
                            && active.messages.len() + self.targets_received >= TARGET_LIMIT)
                    {
                        return Err("History response exceeded its buffer limit.".into());
                    }
                    active.bytes += size;
                    active.messages.push(message.clone());
                }
            }
            BatchEvent::Boundary(BatchBoundary::End(meta)) => {
                self.discarded.remove(&meta.id);
                let key = self
                    .active
                    .iter()
                    .find(|(_, a)| a.batch.as_deref() == Some(&meta.id))
                    .map(|(key, _)| key.clone());
                if let Some(key) = key {
                    let active = self.active.remove(&key).unwrap();
                    if key == "*" {
                        actions.extend(self.complete_targets(active, meta.exhausted));
                    } else {
                        actions.extend(self.complete_history(active, meta.exhausted));
                    }
                }
            }
            BatchEvent::Message {
                message,
                history: false,
                ..
            } => {
                if let Command::Raw(command, args) = &message.command {
                    if command.eq_ignore_ascii_case("FAIL")
                        && args
                            .first()
                            .is_some_and(|a| a.eq_ignore_ascii_case("CHATHISTORY"))
                    {
                        actions.extend(self.fail(args));
                    }
                }
            }
            _ => {}
        }
        actions.extend(self.drain(now));
        Ok(actions)
    }

    fn complete_history(&mut self, active: Active, end: bool) -> Vec<HistoryAction> {
        let count = active.messages.len();
        let last = active.messages.iter().rev().find_map(cursor);
        let progress = last.as_ref().is_some_and(|last| {
            if let Some(id) = active.selector.strip_prefix("msgid=") {
                last.msgid.as_deref().is_some_and(|next| {
                    next != id && !active.pending.selectors.contains(&format!("msgid={next}"))
                })
            } else if let Some(time) = active.selector.strip_prefix("timestamp=") {
                timestamp(&last.time).ok() > timestamp(time).ok()
            } else {
                true
            }
        });
        let exhausted = active.end || end || count < active.page;
        let total = active.pending.total + count;
        let catch_up = active.pending.request.reason == HistoryReason::CatchUp;
        if catch_up {
            self.catch_up_received
                .insert(irc_lower(&active.pending.request.target), total);
        }
        let pending = catch_up && !exhausted && progress && total < HISTORY_LIMIT;
        let mut actions = vec![HistoryAction::Complete {
            target: active.pending.request.target.clone(),
            reason: active.pending.request.reason,
            messages: active.messages,
            exhausted,
            pending,
        }];
        if pending {
            let mut request = active.pending.request;
            request.cursor = last;
            self.waiting.push_back(Pending {
                request,
                total,
                overlap: false,
                selectors: active.pending.selectors,
            });
        } else if catch_up && !exhausted {
            actions.push(HistoryAction::Notice {
                target: Some(active.pending.request.target),
                text: if total >= HISTORY_LIMIT {
                    "Catch-up reached 1000 messages; more history may be available through /history."
                } else {
                    "History made no cursor progress; more may be available through /history."
                }.into(),
            });
        }
        actions
    }

    fn complete_targets(&mut self, active: Active, end: bool) -> Vec<HistoryAction> {
        let count = active.messages.len();
        self.targets_received += count;
        let mut oldest: Option<String> = None;
        let mut actions = Vec::new();
        for message in active.messages {
            if let Command::Raw(command, args) = message.command {
                if command.eq_ignore_ascii_case("CHATHISTORY")
                    && args.len() == 3
                    && args[0].eq_ignore_ascii_case("TARGETS")
                    && timestamp(&args[2]).is_ok()
                    && crate::bouncer::valid_id(&args[1])
                    && args[1] != "*"
                {
                    if oldest
                        .as_ref()
                        .is_none_or(|old| timestamp(&args[2]).ok() < timestamp(old).ok())
                    {
                        oldest = Some(args[2].clone());
                    }
                    if self.targets_seen.len() < TARGET_LIMIT
                        && self.targets_seen.insert(irc_lower(&args[1]))
                    {
                        actions.push(HistoryAction::Target {
                            target: args[1].clone(),
                            latest: args[2].clone(),
                        });
                    }
                }
            }
        }
        let progress = oldest.as_ref().is_some_and(|old| {
            self.targets_range.as_ref().is_some_and(|(newest, oldest)| {
                timestamp(old).ok() < timestamp(newest).ok()
                    && timestamp(old).ok() > timestamp(oldest).ok()
            })
        });
        let more = !active.end && !end && count >= active.page;
        if more && self.targets_received < TARGET_LIMIT && progress {
            if let Some((newest, _)) = &mut self.targets_range {
                *newest = oldest.unwrap()
            }
            self.waiting.push_back(active.pending);
        } else if more {
            actions.push(HistoryAction::Notice {
                target: None,
                text: if self.targets_received >= TARGET_LIMIT {
                    "History target discovery reached 100 conversations; more may be available through /history."
                } else {
                    "History target discovery made no cursor progress; more may be available through /history."
                }.into(),
            });
        }
        actions
    }

    fn fail(&mut self, args: &[String]) -> Vec<HistoryAction> {
        let Some(command) = args.get(2) else {
            return Vec::new();
        };
        // Standard replies put subcommand before target. Missing target context
        // can only identify requests with that subcommand, never another one.
        let target = if command.eq_ignore_ascii_case("TARGETS") {
            Some("*".into())
        } else {
            args.get(3)
                .filter(|target| {
                    args.len() >= 5
                        && !target.starts_with("timestamp=")
                        && !target.starts_with("msgid=")
                })
                .map(|target| irc_lower(target))
        };
        let keys: Vec<_> = self
            .active
            .iter()
            .filter(|(key, active)| {
                if let Some(target) = &target {
                    *key == target
                } else {
                    verb(&active.pending.request).eq_ignore_ascii_case(command) && *key != "*"
                }
            })
            .map(|(key, _)| key.clone())
            .collect();
        let mut actions = Vec::new();
        for key in keys {
            let active = self.active.remove(&key).unwrap();
            self.timed_out.insert(key.clone());
            if let Some(batch) = active.batch {
                self.discarded.insert(batch);
            }
            actions.push(HistoryAction::Notice {
                target: (key != "*").then_some(active.pending.request.target),
                text: "The server rejected the history request.".into(),
            });
        }
        actions
    }

    pub fn tick(&mut self, now: Instant) -> Vec<HistoryAction> {
        let expired: Vec<_> = self
            .active
            .iter()
            .filter(|(_, a)| a.deadline <= now)
            .map(|(key, _)| key.clone())
            .collect();
        let mut actions = Vec::new();
        for key in expired {
            let active = self.active.remove(&key).unwrap();
            self.timed_out.insert(key.clone());
            if let Some(batch) = active.batch {
                self.discarded.insert(batch);
            }
            actions.push(HistoryAction::Notice {
                target: (key != "*").then_some(active.pending.request.target),
                text: "History request timed out; reconnect before retrying.".into(),
            });
        }
        actions.extend(self.drain(now));
        actions
    }

    pub fn cancel(&mut self) {
        self.timed_out.extend(self.active.keys().cloned());
        self.discarded.extend(
            self.active
                .values()
                .filter_map(|active| active.batch.clone()),
        );
        self.active.clear();
        self.waiting.clear();
        self.targets_range = None;
        self.targets_seen.clear();
        self.targets_received = 0;
    }
}

pub fn cursor(message: &Message) -> Option<Cursor> {
    let time = tag(message, "time")?;
    timestamp(time).ok()?;
    Some(Cursor {
        time: time.into(),
        msgid: tag(message, "msgid")
            .filter(|id| valid_msgid(id))
            .map(str::to_owned),
    })
}

#[derive(Debug, Default)]
pub struct Deduper {
    ids: VecDeque<String>,
    id_set: HashSet<String>,
    tuples: HashMap<String, usize>,
    order: VecDeque<String>,
    baseline: Option<HashMap<String, usize>>,
    seen: HashMap<String, usize>,
}

impl Deduper {
    pub fn new(ids: &[String]) -> Self {
        let mut deduper = Self::default();
        for id in ids.iter().rev().take(4096).rev() {
            deduper.insert_id(id)
        }
        deduper
    }

    fn insert_id(&mut self, id: &str) {
        if self.id_set.insert(id.into()) {
            self.ids.push_back(id.into())
        }
        while self.ids.len() > 4096 {
            if let Some(old) = self.ids.pop_front() {
                self.id_set.remove(&old);
            }
        }
    }

    pub fn begin_history(&mut self) {
        self.baseline = Some(self.tuples.clone());
        self.seen.clear();
    }

    pub fn end_history(&mut self) {
        self.baseline = None;
        self.seen.clear();
    }

    pub fn accept(&mut self, message: &Message, history: bool) -> bool {
        if let Some(id) = tag(message, "msgid").filter(|id| valid_msgid(id)) {
            if self.id_set.contains(id) {
                return false;
            }
            self.insert_id(id);
            return true;
        }
        let Some(time) = tag(message, "time").filter(|time| timestamp(time).is_ok()) else {
            return true;
        };
        let signature = format!("{time}|{:?}|{:?}", message.prefix, message.command);
        if history {
            // Only signatures in the bounded snapshot need occurrence counters.
            if let Some(limit) = self
                .baseline
                .as_ref()
                .and_then(|baseline| baseline.get(&signature))
            {
                let occurrence = self.seen.entry(signature.clone()).or_default();
                if *occurrence < *limit {
                    *occurrence += 1;
                    return false;
                }
            }
        }
        *self.tuples.entry(signature.clone()).or_default() += 1;
        self.order.push_back(signature);
        while self.order.len() > 4096 {
            if let Some(old) = self.order.pop_front() {
                if let Some(count) = self.tuples.get_mut(&old) {
                    *count -= 1;
                    if *count == 0 {
                        self.tuples.remove(&old);
                    }
                }
            }
        }
        true
    }

    pub fn recent_ids(&self, limit: usize) -> Vec<String> {
        self.ids
            .iter()
            .skip(self.ids.len().saturating_sub(limit))
            .cloned()
            .collect()
    }
}
