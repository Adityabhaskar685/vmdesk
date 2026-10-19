//! `vmdesk-agent stream`: the NDJSON protocol on stdio, as documented on [`WireMsg`].
//! Only protocol messages go to the output; logs go to stderr.

use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{Instant, interval_at};
use vmdesk_proto::{Snapshot, WireMsg};

use crate::store::Store;

/// How often new events are looked for once the laptop has resumed.
pub const POLL: Duration = Duration::from_secs(1);
const PING_EVERY: Duration = Duration::from_secs(30);
/// Backlog events read from the store at a time.
const PAGE: u32 = 500;

/// Writes `hello`, then serves control messages from `input` until it closes. `since`
/// acts as a `Resume` received before any input.
pub async fn run<R, W, F, Fut>(
    store: &Store,
    hello: &WireMsg,
    since: Option<u64>,
    (input, mut out): (R, W),
    poll: Duration,
    mut snapshot: F,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Snapshot>>,
{
    send(&mut out, hello).await?;
    // Seq of the last event sent; `None` until the laptop resumes.
    let mut cursor = match since {
        Some(since) => Some(catch_up(store, &mut out, since).await?),
        None => None,
    };
    let mut lines = input.lines();
    let mut poll = interval_at(Instant::now() + poll, poll);
    let mut ping = interval_at(Instant::now() + PING_EVERY, PING_EVERY);
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()) };
                match serde_json::from_str::<WireMsg>(&line) {
                    Ok(WireMsg::Resume { since }) => {
                        cursor = Some(catch_up(store, &mut out, since).await?);
                    }
                    Ok(WireMsg::SnapshotRequest) => {
                        if let Some(c) = cursor {
                            cursor = Some(catch_up(store, &mut out, c).await?);
                        }
                        // The last seq the laptop has, so it never skips one recorded
                        // between the catch-up and this message.
                        let seq = match cursor {
                            Some(c) => c,
                            None => store.range().await?.latest,
                        };
                        let data = Box::new(snapshot().await?);
                        send(&mut out, &WireMsg::Snapshot { seq, data }).await?;
                    }
                    Ok(WireMsg::Ping) => send(&mut out, &WireMsg::Pong).await?,
                    Ok(WireMsg::Pong) => {}
                    Ok(other) => tracing::warn!(?other, "ignoring an unexpected message"),
                    Err(_) if line.trim().is_empty() => {}
                    Err(e) => tracing::warn!(error = %e, line, "ignoring a malformed line"),
                }
            }
            _ = poll.tick() => {
                if let Some(c) = cursor {
                    cursor = Some(catch_up(store, &mut out, c).await?);
                }
            }
            _ = ping.tick() => send(&mut out, &WireMsg::Ping).await?,
        }
    }
}

/// Sends every stored event after `since`, a page at a time; returns the new cursor.
async fn catch_up<W: AsyncWrite + Unpin>(store: &Store, out: &mut W, since: u64) -> Result<u64> {
    let mut cursor = since;
    loop {
        let page = store.events_after(cursor, PAGE).await?;
        let full = page.len() == PAGE as usize;
        for event in page {
            cursor = event.seq;
            send(out, &WireMsg::Event(event)).await?;
        }
        if !full {
            return Ok(cursor);
        }
    }
}

async fn send<W: AsyncWrite + Unpin>(out: &mut W, msg: &WireMsg) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    out.write_all(&line).await?;
    out.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream, Lines, duplex};
    use uuid::Uuid;
    use vmdesk_proto::PROTO_VERSION;

    use super::*;
    use crate::home::TempHome;
    use crate::store::Retention;
    use crate::store::tests::{snap, tmux};

    /// The laptop's ends of the agent's stdin and stdout.
    struct Laptop {
        stdin: Option<DuplexStream>,
        stdout: Lines<BufReader<DuplexStream>>,
    }

    impl Laptop {
        async fn send(&mut self, line: &str) {
            let stdin = self.stdin.as_mut().unwrap();
            stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn recv(&mut self) -> WireMsg {
            let line = tokio::time::timeout(Duration::from_secs(5), self.stdout.next_line())
                .await
                .expect("a message within 5 s")
                .unwrap()
                .expect("stream still open");
            serde_json::from_str(&line).unwrap()
        }

        async fn recv_event_seq(&mut self) -> u64 {
            match self.recv().await {
                WireMsg::Event(e) => e.seq,
                other => panic!("expected an event, got {other:?}"),
            }
        }
    }

    async fn store_with_events(tmp: &TempHome, n: usize) -> Store {
        let store = Store::open(&tmp.home().events_db(), Retention::DEFAULT)
            .await
            .unwrap();
        let now = Utc::now();
        let events: Vec<_> = (0..n).map(|i| (now, tmux(&i.to_string()))).collect();
        store.record(&snap("vm", now), &events).await.unwrap();
        store
    }

    fn hello(store_range: crate::store::SeqRange) -> WireMsg {
        WireMsg::Hello {
            agent_id: Uuid::nil(),
            version: "test".into(),
            proto: PROTO_VERSION,
            caps: vec![],
            oldest_seq: store_range.oldest,
            latest_seq: store_range.latest,
            daemon: true,
        }
    }

    /// Runs the stream against `store` while `laptop` drives it; returns the stream's result.
    async fn session<D: Future<Output = ()>>(
        store: &Store,
        poll: Duration,
        laptop: impl FnOnce(Laptop) -> D,
    ) -> Result<()> {
        let (stdin_laptop, stdin_agent) = duplex(1 << 16);
        let (stdout_agent, stdout_laptop) = duplex(1 << 16);
        let hello = hello(store.range().await.unwrap());
        let agent = run(
            store,
            &hello,
            None,
            (BufReader::new(stdin_agent), stdout_agent),
            poll,
            || async { Ok(snap("fresh", Utc::now())) },
        );
        let laptop = laptop(Laptop {
            stdin: Some(stdin_laptop),
            stdout: BufReader::new(stdout_laptop).lines(),
        });
        let (result, ()) = tokio::join!(agent, laptop);
        result
    }

    #[tokio::test]
    async fn resume_sends_backlog_then_live_events_until_stdin_closes() {
        let tmp = TempHome::new();
        let store = &store_with_events(&tmp, 3).await;
        let result = session(store, Duration::from_millis(50), |mut laptop| async move {
            let WireMsg::Hello {
                latest_seq,
                oldest_seq,
                ..
            } = laptop.recv().await
            else {
                panic!("hello first");
            };
            assert_eq!((oldest_seq, latest_seq), (1, 3));

            laptop.send("this is not json").await;
            laptop.send(r#"{"t":"no_such_message"}"#).await;
            laptop.send(r#"{"t":"ping"}"#).await;
            assert_eq!(laptop.recv().await, WireMsg::Pong, "bad lines are skipped");

            laptop.send(r#"{"t":"resume","since":1}"#).await;
            assert_eq!(laptop.recv_event_seq().await, 2);
            assert_eq!(laptop.recv_event_seq().await, 3);

            let now = Utc::now();
            store
                .record(&snap("vm", now), &[(now, tmux("live"))])
                .await
                .unwrap();
            assert_eq!(laptop.recv_event_seq().await, 4, "recorded while streaming");

            drop(laptop.stdin.take());
            assert_eq!(
                laptop.stdout.next_line().await.unwrap(),
                None,
                "stream ends"
            );
        })
        .await;
        result.unwrap();
    }

    #[tokio::test]
    async fn snapshot_request_flushes_pending_events_first() {
        let tmp = TempHome::new();
        let store = &store_with_events(&tmp, 2).await;
        // No poll tick during the test: only the snapshot request can send event 3.
        let result = session(store, Duration::from_hours(1), |mut laptop| async move {
            assert!(matches!(laptop.recv().await, WireMsg::Hello { .. }));
            laptop.send(r#"{"t":"resume","since":0}"#).await;
            assert_eq!(laptop.recv_event_seq().await, 1);
            assert_eq!(laptop.recv_event_seq().await, 2);

            let now = Utc::now();
            store
                .record(&snap("vm", now), &[(now, tmux("pending"))])
                .await
                .unwrap();
            laptop.send(r#"{"t":"snapshot_request"}"#).await;
            assert_eq!(laptop.recv_event_seq().await, 3);
            match laptop.recv().await {
                WireMsg::Snapshot { seq, data } => {
                    assert_eq!(seq, 3);
                    assert_eq!(
                        data.system.hostname, "fresh",
                        "taken now, not the stored one"
                    );
                }
                other => panic!("expected the snapshot, got {other:?}"),
            }
            drop(laptop.stdin.take());
        })
        .await;
        result.unwrap();
    }

    #[tokio::test]
    async fn backlog_larger_than_a_page_arrives_in_order() {
        let tmp = TempHome::new();
        let store = store_with_events(&tmp, 1200).await;
        let result = session(&store, Duration::from_hours(1), |mut laptop| async move {
            laptop.recv().await;
            laptop.send(r#"{"t":"resume","since":100}"#).await;
            for expected in 101..=1200 {
                assert_eq!(laptop.recv_event_seq().await, expected);
            }
            laptop.send(r#"{"t":"snapshot_request"}"#).await;
            assert!(matches!(
                laptop.recv().await,
                WireMsg::Snapshot { seq: 1200, .. }
            ));
            drop(laptop.stdin.take());
        })
        .await;
        result.unwrap();
    }
}
