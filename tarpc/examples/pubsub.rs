// Copyright 2018 Google LLC
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

/// - The PubSub server sets up TCP listeners on 2 ports, the "subscriber" port and the "publisher"
///   port. Because both publishers and subscribers initiate their connections to the PubSub
///   server, the server requires no prior knowledge of either publishers or subscribers.
///
/// - Subscribers connect to the server on the server's "subscriber" port. Once a connection is
///   established, the server acts as the client of the Subscriber service, initially requesting
///   the topics the subscriber is interested in, and subsequently sending topical messages to the
///   subscriber.
///
/// - Publishers connect to the server on the "publisher" port and, once connected, they send
///   topical messages via Publisher service to the server. The server then broadcasts each
///   messages to all clients subscribed to the topic of that message.
///
///       Subscriber                        Publisher                       PubSub Server
/// T1        |                                 |                                 |             
/// T2        |-----Connect------------------------------------------------------>|
/// T3        |                                 |                                 |
/// T2        |<-------------------------------------------------------Topics-----|
/// T2        |-----(OK) Topics-------------------------------------------------->|
/// T3        |                                 |                                 |
/// T4        |                                 |-----Connect-------------------->|
/// T5        |                                 |                                 |
/// T6        |                                 |-----Publish-------------------->|
/// T7        |                                 |                                 |
/// T8        |<------------------------------------------------------Receive-----|
/// T9        |-----(OK) Receive------------------------------------------------->|
/// T10       |                                 |                                 |
/// T11       |                                 |<--------------(OK) Publish------|
use anyhow::anyhow;
use futures::{
    channel::oneshot,
    future::{self, AbortHandle},
    prelude::*,
};
use log::info;
use publisher::Publisher as _;
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
};
use subscriber::Subscriber as _;
use tarpc::{
    client, context,
    serde_transport::tcp,
    server::{self, Channel},
};
use tokio::net::ToSocketAddrs;
use tokio_serde::formats::Json;

pub mod subscriber {
    #[tarpc::service]
    pub trait Subscriber {
        async fn topics() -> Vec<String>;
        async fn receive(topic: String, message: String);
    }
}

pub mod publisher {
    #[tarpc::service]
    pub trait Publisher {
        async fn publish(topic: String, message: String);
    }
}

#[derive(Clone, Debug)]
struct Subscriber {
    local_addr: SocketAddr,
    topics: Vec<String>,
}

#[tarpc::server]
impl subscriber::Subscriber for Subscriber {
    async fn topics(self, _: context::Context) -> Vec<String> {
        self.topics.clone()
    }

    async fn receive(self, _: context::Context, topic: String, message: String) {
        info!(
            "[{}] received message on topic '{}': {}",
            self.local_addr, topic, message
        );
    }
}

struct SubscriberHandle(AbortHandle);

impl Drop for SubscriberHandle {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Subscriber {
    async fn connect(
        publisher_addr: impl ToSocketAddrs,
        topics: Vec<String>,
    ) -> anyhow::Result<SubscriberHandle> {
        let publisher = tcp::connect(publisher_addr, Json::default()).await?;
        let local_addr = publisher.local_addr()?;
        let mut handler = server::BaseChannel::with_defaults(publisher)
            .respond_with(Subscriber { local_addr, topics }.serve());
        // The first request is for the topics being subscriibed to.
        match handler.next().await {
            Some(init_topics) => init_topics?.await,
            None => {
                return Err(anyhow!(
                    "[{}] Server never initialized the subscriber.",
                    local_addr
                ))
            }
        };
        let (handler, abort_handle) = future::abortable(handler.execute());
        tokio::spawn(async move {
            match handler.await {
                Ok(()) | Err(future::Aborted) => info!("[{}] subscriber shutdown.", local_addr),
            }
        });
        Ok(SubscriberHandle(abort_handle))
    }
}

#[derive(Debug)]
struct Subscription {
    subscriber: subscriber::SubscriberClient,
    topics: Vec<String>,
}

#[derive(Clone, Debug)]
struct Publisher {
    clients: Arc<Mutex<HashMap<SocketAddr, Subscription>>>,
    subscriptions: Arc<RwLock<HashMap<String, HashMap<SocketAddr, subscriber::SubscriberClient>>>>,
}

struct PublisherAddrs {
    publisher: SocketAddr,
    subscriptions: SocketAddr,
}

impl Publisher {
    async fn start(self) -> io::Result<PublisherAddrs> {
        let mut connecting_publishers = tcp::listen("localhost:0", Json::default).await?;

        let publisher_addrs = PublisherAddrs {
            publisher: connecting_publishers.local_addr(),
            subscriptions: self.clone().start_subscription_manager().await?,
        };

        info!("[{}] listening for publishers.", publisher_addrs.publisher);
        tokio::spawn(async move {
            // Because this is just an example, we know there will only be one publisher. In more
            // realistic code, this would be a loop to continually accept new publisher
            // connections.
            let publisher = connecting_publishers.next().await.unwrap().unwrap();
            info!("[{}] publisher connected.", publisher.peer_addr().unwrap());

            server::BaseChannel::with_defaults(publisher)
                .respond_with(self.serve())
                .execute()
                .await
        });

        Ok(publisher_addrs)
    }

    async fn start_subscription_manager(mut self) -> io::Result<SocketAddr> {
        let mut connecting_subscribers = tcp::listen("localhost:0", Json::default)
            .await?
            .filter_map(|r| future::ready(r.ok()));
        let new_subscriber_addr = connecting_subscribers.get_ref().local_addr();
        info!("[{}] listening for subscribers.", new_subscriber_addr);

        tokio::spawn(async move {
            while let Some(conn) = connecting_subscribers.next().await {
                let subscriber_addr = conn.peer_addr().unwrap();

                let tarpc::client::NewClient {
                    client: subscriber,
                    dispatch,
                } = subscriber::SubscriberClient::new(client::Config::default(), conn);
                let (ready_tx, ready) = oneshot::channel();
                self.clone()
                    .start_subscriber_gc(subscriber_addr, dispatch, ready);

                // Populate the topics
                self.initialize_subscription(subscriber_addr, subscriber)
                    .await;

                // Signal that initialization is done.
                ready_tx.send(()).unwrap();
            }
        });

        Ok(new_subscriber_addr)
    }

    async fn initialize_subscription(
        &mut self,
        subscriber_addr: SocketAddr,
        mut subscriber: subscriber::SubscriberClient,
    ) {
        // Populate the topics
        if let Ok(topics) = subscriber.topics(context::current()).await {
            self.clients.lock().unwrap().insert(
                subscriber_addr,
                Subscription {
                    subscriber: subscriber.clone(),
                    topics: topics.clone(),
                },
            );

            info!("[{}] subscribed to topics: {:?}", subscriber_addr, topics);
            let mut subscriptions = self.subscriptions.write().unwrap();
            for topic in topics {
                subscriptions
                    .entry(topic)
                    .or_insert_with(HashMap::new)
                    .insert(subscriber_addr, subscriber.clone());
            }
        }
    }

    fn start_subscriber_gc(
        self,
        subscriber_addr: SocketAddr,
        client_dispatch: impl Future<Output = anyhow::Result<()>> + Send + 'static,
        subscriber_ready: oneshot::Receiver<()>,
    ) {
        tokio::spawn(async move {
            if let Err(e) = client_dispatch.await {
                info!(
                    "[{}] subscriber connection broken: {:?}",
                    subscriber_addr, e
                )
            }
            // Don't clean up the subscriber until initialization is done.
            let _ = subscriber_ready.await;
            if let Some(subscription) = self.clients.lock().unwrap().remove(&subscriber_addr) {
                info!(
                    "[{} unsubscribing from topics: {:?}",
                    subscriber_addr, subscription.topics
                );
                let mut subscriptions = self.subscriptions.write().unwrap();
                for topic in subscription.topics {
                    let subscribers = subscriptions.get_mut(&topic).unwrap();
                    subscribers.remove(&subscriber_addr);
                    if subscribers.is_empty() {
                        subscriptions.remove(&topic);
                    }
                }
            }
        });
    }
}

#[tarpc::server]
impl publisher::Publisher for Publisher {
    async fn publish(self, _: context::Context, topic: String, message: String) {
        info!("received message to publish.");
        let mut subscribers = match self.subscriptions.read().unwrap().get(&topic) {
            None => return,
            Some(subscriptions) => subscriptions.clone(),
        };
        let mut publications = Vec::new();
        for client in subscribers.values_mut() {
            publications.push(client.receive(context::current(), topic.clone(), message.clone()));
        }
        // Ignore failing subscribers. In a real pubsub, you'd want to continually retry until
        // subscribers ack. Of course, a lot would be different in a real pubsub :)
        for response in future::join_all(publications).await {
            if let Err(e) = response {
                info!("failed to broadcast to subscriber: {}", e);
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let clients = Arc::new(Mutex::new(HashMap::new()));
    let addrs = Publisher {
        clients,
        subscriptions: Arc::new(RwLock::new(HashMap::new())),
    }
    .start()
    .await?;

    let _subscriber0 = Subscriber::connect(
        addrs.subscriptions,
        vec!["calculus".into(), "cool shorts".into()],
    )
    .await?;

    let _subscriber1 = Subscriber::connect(
        addrs.subscriptions,
        vec!["cool shorts".into(), "history".into()],
    )
    .await?;

    let mut publisher = publisher::PublisherClient::new(
        client::Config::default(),
        tcp::connect(addrs.publisher, Json::default()).await?,
    )
    .spawn()?;

    publisher
        .publish(context::current(), "calculus".into(), "sqrt(2)".into())
        .await?;

    publisher
        .publish(
            context::current(),
            "cool shorts".into(),
            "hello to all".into(),
        )
        .await?;

    publisher
        .publish(context::current(), "history".into(), "napoleon".to_string())
        .await?;

    drop(_subscriber0);

    publisher
        .publish(
            context::current(),
            "cool shorts".into(),
            "hello to who?".into(),
        )
        .await?;

    info!("done.");

    Ok(())
}
