use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use proptest::prelude::*;
use tokio::sync::mpsc;

use zatat_channels::{Channel, ChannelManager};
use zatat_connection::{ConnectionHandle, Outbound};
use zatat_core::application::{AcceptClientEventsFrom, Application};
use zatat_core::channel_name::ChannelName;
use zatat_core::id::{AppId, AppKey, SocketId};
use zatat_protocol::presence::PresenceMember;

fn mk_app() -> Arc<Application> {
    Arc::new(
        Application::new(
            AppId::from("app"),
            AppKey::from("key"),
            "secret".into(),
            30,
            30,
            10_000,
            None,
            AcceptClientEventsFrom::Members,
            None,
            vec!["*".into()],
        )
        .unwrap(),
    )
}

fn mk_handle(id: &str) -> (ConnectionHandle, mpsc::Receiver<Outbound>) {
    let (tx, rx) = mpsc::channel(64);
    (
        ConnectionHandle::from_parts(
            SocketId::from_string(id.into()),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        ),
        rx,
    )
}

#[derive(Debug, Clone)]
enum Op {
    Sub { socket: u8, channel: u8, user: u8 },
    Unsub { socket: u8, channel: u8 },
    Broadcast { channel: u8, except: Option<u8> },
}

// A small alphabet so we actually hit subscribe/unsubscribe on the same pair.
const SOCKETS: u8 = 6;
const CHANNELS: u8 = 3;
const USERS: u8 = 4;

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..SOCKETS, 0..CHANNELS, 0..USERS).prop_map(|(socket, channel, user)| Op::Sub {
            socket,
            channel,
            user
        }),
        (0..SOCKETS, 0..CHANNELS).prop_map(|(socket, channel)| Op::Unsub { socket, channel }),
        (0..CHANNELS, proptest::option::of(0..SOCKETS))
            .prop_map(|(channel, except)| Op::Broadcast { channel, except }),
    ]
}

fn channel_name(i: u8) -> ChannelName {
    // Mix of kinds so we exercise public + presence.
    match i % CHANNELS {
        0 => ChannelName::new(format!("public-{i}")),
        _ => ChannelName::new(format!("presence-room-{i}")),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn channel_manager_invariants(ops in proptest::collection::vec(op_strategy(), 1..80)) {
        let app = mk_app();
        let mgr = ChannelManager::new();

        // Model: the set of (socket_id, channel_name) subscriptions that
        // SHOULD exist after replaying `ops`, plus per-channel user_id
        // refcounts for presence channels.
        let mut subs: BTreeSet<(u8, String)> = BTreeSet::new();
        // (channel_name, user_id) -> refcount of active connections
        let mut user_refs: BTreeMap<(String, u8), usize> = BTreeMap::new();
        // Per-socket presence tag — what user did each socket subscribe as
        // on each channel?
        let mut presence_tags: BTreeMap<(u8, String), u8> = BTreeMap::new();

        // Live mpsc receivers so we can inspect who got broadcasts.
        let mut rxs: BTreeMap<u8, mpsc::Receiver<Outbound>> = BTreeMap::new();
        let mut handles: BTreeMap<u8, ConnectionHandle> = BTreeMap::new();

        let mut broadcast_seq = 0u64;
        for op in &ops {
            match *op {
                Op::Sub { socket, channel, user } => {
                    let name = channel_name(channel);
                    let is_presence = name.kind().is_presence();

                    // Make (or reuse) a handle for this socket.
                    let handle = handles.entry(socket).or_insert_with(|| {
                        let (h, rx) = mk_handle(&format!("sock-{socket}"));
                        rxs.insert(socket, rx);
                        h
                    }).clone();
                    mgr.register_connection(app.clone(), handle.clone()).ok();

                    let presence = if is_presence {
                        Some(PresenceMember {
                            user_id: format!("u-{user}"),
                            user_info: None,
                        })
                    } else {
                        None
                    };
                    mgr.subscribe(
                        &app,
                        &name,
                        SocketId::from_string(format!("sock-{socket}")),
                        handle,
                        presence.clone(),
                    );

                    let key = (socket, name.as_str().to_string());
                    let newly = subs.insert(key.clone());
                    // same socket is a no-op. Only update model state when
                    // this is a genuinely new membership.
                    if is_presence && newly {
                        *user_refs.entry((name.as_str().to_string(), user)).or_insert(0) += 1;
                        presence_tags.insert(key, user);
                    }
                }

                Op::Unsub { socket, channel } => {
                    let name = channel_name(channel);
                    mgr.unsubscribe(&app.id, name.as_str(), &SocketId::from_string(format!("sock-{socket}")));
                    let key = (socket, name.as_str().to_string());
                    let was_subscribed = subs.remove(&key);
                    if was_subscribed && name.kind().is_presence() {
                        if let Some(user) = presence_tags.remove(&key) {
                            if let Some(n) = user_refs.get_mut(&(name.as_str().to_string(), user)) {
                                *n -= 1;
                                if *n == 0 {
                                    user_refs.remove(&(name.as_str().to_string(), user));
                                }
                            }
                        }
                    }
                }

                Op::Broadcast { channel, except } => {
                    let name = channel_name(channel);
                    broadcast_seq += 1;
                    // Unique payload so we can check "this specific broadcast
                    // never reached the except socket" without confusing it
                    // with earlier broadcasts sitting in the receiver queue.
                    let marker = format!("bx-{broadcast_seq}-{channel}");
                    if let Some(ch) = mgr.find_channel(&app.id, name.as_str()) {
                        let except_id = except.map(|s| SocketId::from_string(format!("sock-{s}")));
                        ch.broadcast(
                            Arc::from(marker.clone().into_boxed_str()),
                            except_id.as_ref(),
                        );

                        if let Some(ex) = except {
                            // Only subscribers of THIS channel were candidate
                            // recipients; if the except socket is subscribed,
                            // drain its queue and make sure our unique
                            // marker is never in there.
                            let was_subscribed = subs
                                .iter()
                                .any(|(s, c)| *s == ex && c == name.as_str());
                            if was_subscribed {
                                if let Some(rx) = rxs.get_mut(&ex) {
                                    while let Ok(msg) = rx.try_recv() {
                                        if let Outbound::Text(s) = msg {
                                            prop_assert_ne!(
                                                &*s, &*marker,
                                                "except socket {} received excluded broadcast",
                                                ex
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // --- After every op, the invariants must hold ---------------

            // Invariant 1: for each channel the model agrees on subscriber count.
            let channels_in_model: BTreeSet<String> =
                subs.iter().map(|(_, c)| c.clone()).collect();
            for ch_name in &channels_in_model {
                let model_count = subs.iter().filter(|(_, c)| c == ch_name).count();
                let live: Option<Arc<Channel>> = mgr.find_channel(&app.id, ch_name);
                prop_assert!(live.is_some(), "channel {ch_name} missing from manager");
                let live = live.unwrap();
                prop_assert_eq!(
                    live.len(),
                    model_count,
                    "subscriber count mismatch on {}",
                    ch_name
                );
            }

            // Invariant 2: presence user_count matches distinct user_ids.
            for ((ch_name, _user), _) in user_refs.iter() {
                let distinct_users = user_refs
                    .iter()
                    .filter(|((c, _), _)| c == ch_name)
                    .count();
                let live = mgr.find_channel(&app.id, ch_name).expect("channel exists");
                prop_assert_eq!(
                    live.user_count(),
                    distinct_users,
                    "user_count mismatch on {}",
                    ch_name
                );
            }

            // Invariant 3: empty channels are removed.
            for ch_name in channels_in_model.iter() {
                let has_subs = subs.iter().any(|(_, c)| c == ch_name);
                if !has_subs {
                    prop_assert!(
                        mgr.find_channel(&app.id, ch_name).is_none(),
                        "empty channel {ch_name} not removed"
                    );
                }
            }
        }
    }
}
