//! Sharding of `accountSubscribe` over several connections, against a local mock server.

use cb_feed::ws::Watch;
use cb_feed::WsFeed;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone, PartialEq)]
enum Ev {
    Sub { conn: usize, acct: [u8; 32], sub_id: u64 },
    Unsub { conn: usize, sub_id: u64 },
}

type Log = Arc<Mutex<Vec<Ev>>>;

/// Mock RPC: confirms every subscribe with a unique id after `delay`, records everything.
async fn mock(delay: Duration) -> (String, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let log: Log = Arc::default();
    let l = log.clone();
    tokio::spawn(async move {
        for conn in 0..usize::MAX {
            let (stream, _) = listener.accept().await.unwrap();
            let l = l.clone();
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let mut next_sub = 100 * (conn as u64 + 1);
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                    let id = v["id"].as_u64().unwrap();
                    let result = match v["method"].as_str().unwrap() {
                        "accountSubscribe" => {
                            let mut acct = [0u8; 32];
                            acct.copy_from_slice(&bs58::decode(v["params"][0].as_str().unwrap()).into_vec().unwrap());
                            next_sub += 1;
                            l.lock().unwrap().push(Ev::Sub { conn, acct, sub_id: next_sub });
                            tokio::time::sleep(delay).await;
                            serde_json::json!(next_sub)
                        }
                        "accountUnsubscribe" => {
                            let sub_id = v["params"][0].as_u64().unwrap();
                            l.lock().unwrap().push(Ev::Unsub { conn, sub_id });
                            serde_json::json!(true)
                        }
                        m => panic!("unexpected {m}"),
                    };
                    let reply = serde_json::json!({"jsonrpc": "2.0", "result": result, "id": id});
                    ws.send(Message::Text(reply.to_string().into())).await.unwrap();
                }
            });
        }
    });
    (url, log)
}

async fn until(log: &Log, what: &str, f: impl Fn(&[Ev]) -> bool) {
    for _ in 0..200 {
        if f(&log.lock().unwrap()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}; log = {:?}", log.lock().unwrap());
}

fn accts_on(log: &[Ev], c: usize) -> HashSet<u8> {
    log.iter()
        .filter_map(|e| match e {
            Ev::Sub { conn, acct, .. } if *conn == c => Some(acct[0]),
            _ => None,
        })
        .collect()
}

fn conns(log: &[Ev]) -> HashSet<usize> {
    log.iter().map(|e| match e { Ev::Sub { conn, .. } | Ev::Unsub { conn, .. } => *conn }).collect()
}

#[tokio::test]
async fn accounts_over_the_limit_open_more_connections_and_drops_hit_the_owner() {
    let (url, log) = mock(Duration::ZERO).await;
    let feed = WsFeed::new(url);
    let a = |n: u8| [n; 32];
    // 7 accounts at 3 per connection: three connections (3, 3, 1).
    let (_rx, ctl) = feed.spawn_controlled_with_limit((1..=7).map(a).collect(), 3);
    until(&log, "7 subscribes", |l| l.len() == 7).await;
    assert_eq!(conns(&log.lock().unwrap()).len(), 3);

    // Two more fit on the connection that has room; no new connection.
    ctl.send(Watch::Add(vec![a(8), a(9)])).unwrap();
    until(&log, "9 subscribes", |l| l.len() == 9).await;
    {
        let l = log.lock().unwrap();
        assert_eq!(conns(&l).len(), 3, "room existed; log {l:?}");
        let home7 = conns(&l).into_iter().find(|c| accts_on(&l, *c).contains(&7)).unwrap();
        assert_eq!(accts_on(&l, home7), HashSet::from([7, 8, 9]), "added to the shard with room");
    }

    // Everything is full now: the next account opens a fourth connection.
    ctl.send(Watch::Add(vec![a(10)])).unwrap();
    until(&log, "10 subscribes", |l| l.len() == 10).await;
    assert_eq!(conns(&log.lock().unwrap()).len(), 4);

    // A drop unsubscribes on the connection that owns the account, with its subscription id.
    let (owner, sub_id) = {
        let l = log.lock().unwrap();
        l.iter().find_map(|e| match e { Ev::Sub { conn, acct, sub_id } if acct[0] == 2 => Some((*conn, *sub_id)), _ => None }).unwrap()
    };
    ctl.send(Watch::Drop(vec![a(2)])).unwrap();
    until(&log, "the unsubscribe", |l| l.iter().any(|e| matches!(e, Ev::Unsub { .. }))).await;
    let unsubs: Vec<Ev> = log.lock().unwrap().iter().filter(|e| matches!(e, Ev::Unsub { .. })).cloned().collect();
    assert_eq!(unsubs, vec![Ev::Unsub { conn: owner, sub_id }]);

    // Add after Drop: the freed slot is reused, on the same shard, no fifth connection.
    ctl.send(Watch::Add(vec![a(2)])).unwrap();
    until(&log, "re-subscribe", |l| l.iter().filter(|e| matches!(e, Ev::Sub { acct, .. } if acct[0] == 2)).count() == 2).await;
    let l = log.lock().unwrap();
    assert_eq!(conns(&l).len(), 4);
    assert!(accts_on(&l, owner).contains(&2));
}

#[tokio::test]
async fn an_account_dropped_before_its_confirmation_is_unsubscribed_when_it_arrives() {
    // The server answers slowly, so the drop lands while the subscribe is still pending.
    let (url, log) = mock(Duration::from_millis(300)).await;
    let feed = WsFeed::new(url);
    let (_rx, ctl) = feed.spawn_controlled_with_limit(vec![[1; 32]], 3);
    until(&log, "the subscribe", |l| l.len() == 1).await;
    ctl.send(Watch::Drop(vec![[1; 32]])).unwrap();
    until(&log, "an unsubscribe for the late confirmation", |l| {
        l.iter().any(|e| matches!(e, Ev::Unsub { sub_id: 101, .. }))
    })
    .await;
}
