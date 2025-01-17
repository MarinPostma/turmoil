use crate::envelope::{Envelope, Protocol};
use crate::host::Host;
use crate::rt::Rt;
use crate::{config, TRACING_TARGET};

use indexmap::IndexMap;
use rand::{Rng, RngCore};
use rand_distr::{Distribution, Exp};
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::time::Instant;

/// Describes the network topology.
pub(crate) struct Topology {
    config: config::Link,

    /// Specific configuration overrides between specific hosts.
    links: IndexMap<Pair, Link>,

    /// We don't use a Rt for async. Right now, we just use it to tick time
    /// forward in the same way we do it elsewhere. We'd like to represent
    /// network state with async in the future.
    rt: Rt,
}

/// This type is used as the key in the [`Topology::links`] map. See [`new`]
/// which orders the addrs, such that this type uniquely identifies the link
/// between two hosts on the network.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct Pair(IpAddr, IpAddr);

impl Pair {
    fn new(a: IpAddr, b: IpAddr) -> Pair {
        assert_ne!(a, b);

        if a < b {
            Pair(a, b)
        } else {
            Pair(b, a)
        }
    }
}

/// A two-way link between two hosts on the network.
struct Link {
    state: State,

    /// Optional, per-link configuration.
    config: config::Link,

    /// Inflight messages that are either scheduled for delivery in the future
    /// or are on hold.
    inflight: VecDeque<Inflight>,

    /// Messages that are ready to be delivered.
    deliverable: IndexMap<IpAddr, VecDeque<Envelope>>,

    /// The current network time, moved forward with [`Link::tick`].
    now: Instant,
}

enum State {
    /// The link is healthy.
    Healthy,

    /// The link was explicitly partitioned.
    ExplicitPartition,

    /// The link was randomly partitioned.
    RandPartition,

    /// Messages are being held indefinitely.
    Hold,
}

impl Topology {
    pub(crate) fn new(config: config::Link) -> Topology {
        Topology {
            config,
            links: IndexMap::new(),
            rt: Rt::new(),
        }
    }

    /// Register a link between two hosts
    pub(crate) fn register(&mut self, a: IpAddr, b: IpAddr) {
        let pair = Pair::new(a, b);
        assert!(self
            .links
            .insert(pair.clone(), Link::new(self.rt.now()))
            .is_none());
    }

    pub(crate) fn set_max_message_latency(&mut self, value: Duration) {
        self.config.latency_mut().max_message_latency = value;
    }

    pub(crate) fn set_link_max_message_latency(&mut self, a: IpAddr, b: IpAddr, value: Duration) {
        self.links[&Pair::new(a, b)]
            .latency(self.config.latency())
            .max_message_latency = value;
    }

    pub(crate) fn set_message_latency_curve(&mut self, value: f64) {
        self.config.latency_mut().latency_distribution = Exp::new(value).unwrap();
    }

    pub(crate) fn set_fail_rate(&mut self, value: f64) {
        self.config.message_loss_mut().fail_rate = value;
    }

    pub(crate) fn set_link_fail_rate(&mut self, a: IpAddr, b: IpAddr, value: f64) {
        self.links[&Pair::new(a, b)]
            .message_loss(&self.config.message_loss())
            .fail_rate = value;
    }

    // Send a `message` from `src` to `dst`. This method returns immediately,
    // and message delivery happens at a later time (or never, if the link is
    // broken).
    pub(crate) fn enqueue_message(
        &mut self,
        rand: &mut dyn RngCore,
        src: SocketAddr,
        dst: SocketAddr,
        message: Protocol,
    ) {
        let link = &mut self.links[&Pair::new(src.ip(), dst.ip())];
        link.enqueue_message(&self.config, rand, src, dst, message);
    }

    // Move messages from any network links to the `dst` host.
    pub(crate) fn deliver_messages(&mut self, rand: &mut dyn RngCore, dst: &mut Host) {
        for (pair, link) in &mut self.links {
            if pair.0 == dst.addr || pair.1 == dst.addr {
                link.deliver_messages(&self.config, rand, dst);
            }
        }
    }

    pub(crate) fn hold(&mut self, a: IpAddr, b: IpAddr) {
        self.links[&Pair::new(a, b)].hold();
    }

    pub(crate) fn release(&mut self, a: IpAddr, b: IpAddr) {
        self.links[&Pair::new(a, b)].release();
    }

    pub(crate) fn partition(&mut self, a: IpAddr, b: IpAddr) {
        self.links[&Pair::new(a, b)].explicit_partition();
    }

    pub(crate) fn repair(&mut self, a: IpAddr, b: IpAddr) {
        self.links[&Pair::new(a, b)].explicit_repair();
    }

    pub(crate) fn tick_by(&mut self, duration: Duration) {
        self.rt.tick(duration);
        for link in self.links.values_mut() {
            link.tick(self.rt.now());
        }
    }
}

struct Inflight {
    args: InflightArgs,
    status: DeliveryStatus,
}

struct InflightArgs {
    src: SocketAddr,
    dst: SocketAddr,
    message: Protocol,
}

enum DeliveryStatus {
    DeliverAfter(Instant),
    Hold,
}

impl Link {
    fn new(now: Instant) -> Link {
        Link {
            state: State::Healthy,
            config: config::Link::default(),
            inflight: VecDeque::new(),
            deliverable: IndexMap::new(),
            now,
        }
    }

    fn enqueue_message(
        &mut self,
        global_config: &config::Link,
        rand: &mut dyn RngCore,
        src: SocketAddr,
        dst: SocketAddr,
        message: Protocol,
    ) {
        tracing::trace!(target: TRACING_TARGET, ?src, ?dst, protocol = %message, "Send");

        self.rand_partition_or_repair(global_config, rand);
        self.enqueue(global_config, rand, src, dst, message);
        self.process_deliverables();
    }

    // src -> link -> dst
    //        ^-- you are here!
    //
    // Messages may be dropped, sit on the link for a while (due to latency, or
    // because the link has stalled), or be delivered immediately.
    fn enqueue(
        &mut self,
        global_config: &config::Link,
        rand: &mut dyn RngCore,
        src: SocketAddr,
        dst: SocketAddr,
        message: Protocol,
    ) {
        let status = match self.state {
            State::Healthy => {
                let delay = self.delay(global_config.latency(), rand);
                DeliveryStatus::DeliverAfter(self.now + delay)
            }
            State::Hold => {
                tracing::trace!(target: TRACING_TARGET,?src, ?dst, protocol = %message, "Hold");

                DeliveryStatus::Hold
            }
            _ => {
                tracing::trace!(target: TRACING_TARGET,?src, ?dst, protocol = %message, "Drop");

                return;
            }
        };

        let inflight = Inflight {
            args: InflightArgs { src, dst, message },
            status,
        };

        self.inflight.push_back(inflight);
    }

    fn tick(&mut self, now: Instant) {
        self.now = now;
        self.process_deliverables();
    }

    fn process_deliverables(&mut self) {
        // TODO: `drain_filter` is not yet stable, and so we have a low quality
        // implementation here that avoids clones.
        let mut sent = 0;
        for i in 0..self.inflight.len() {
            let index = i - sent;
            let inflight = &self.inflight[index];
            if let DeliveryStatus::DeliverAfter(time) = inflight.status {
                if time <= self.now {
                    let inflight = self.inflight.remove(index).unwrap();
                    let envelope = Envelope {
                        src: inflight.args.src,
                        dst: inflight.args.dst,
                        message: inflight.args.message,
                    };
                    self.deliverable
                        .entry(inflight.args.dst.ip())
                        .or_default()
                        .push_back(envelope);
                    sent += 1;
                }
            }
        }
    }

    // FIXME: This implementation does not respect message delivery order. If
    // host A and host B are ordered (by addr), and B sends before A, then this
    // method will deliver A's message before B's.
    fn deliver_messages(
        &mut self,
        global_config: &config::Link,
        rand: &mut dyn RngCore,
        host: &mut Host,
    ) {
        let deliverable = self
            .deliverable
            .entry(host.addr)
            .or_default()
            .drain(..)
            .collect::<Vec<Envelope>>();

        for message in deliverable {
            let (src, dst) = (message.src, message.dst);
            if let Err(message) = host.receive_from_network(message) {
                self.enqueue_message(global_config, rand, dst, src, message);
            }
        }
    }

    // Randomly break or repair this link.
    fn rand_partition_or_repair(&mut self, global_config: &config::Link, rand: &mut dyn RngCore) {
        match self.state {
            State::Healthy => {
                if self.rand_partition(global_config.message_loss(), rand) {
                    self.state = State::RandPartition;
                }
            }
            State::RandPartition => {
                if self.rand_repair(global_config.message_loss(), rand) {
                    self.release();
                }
            }
            _ => {}
        }
    }

    fn hold(&mut self) {
        self.state = State::Hold;
    }

    // This link becomes healthy, and any held messages are scheduled for delivery.
    fn release(&mut self) {
        self.state = State::Healthy;
        for inflight in &mut self.inflight {
            if let DeliveryStatus::Hold = inflight.status {
                inflight.status = DeliveryStatus::DeliverAfter(self.now);
            }
        }
    }

    fn explicit_partition(&mut self) {
        self.state = State::ExplicitPartition;
    }

    // Repair the link, without releasing any held messages.
    fn explicit_repair(&mut self) {
        self.state = State::Healthy;
    }

    /// Should the link be randomly partitioned
    fn rand_partition(&self, global: &config::MessageLoss, rand: &mut dyn RngCore) -> bool {
        let config = self.config.message_loss.as_ref().unwrap_or(global);
        let fail_rate = config.fail_rate;
        fail_rate > 0.0 && rand.gen_bool(fail_rate)
    }

    fn rand_repair(&self, global: &config::MessageLoss, rand: &mut dyn RngCore) -> bool {
        let config = self.config.message_loss.as_ref().unwrap_or(global);
        let repair_rate = config.repair_rate;
        repair_rate > 0.0 && rand.gen_bool(repair_rate)
    }

    fn delay(&self, global: &config::Latency, rand: &mut dyn RngCore) -> Duration {
        let config = self.config.latency.as_ref().unwrap_or(global);

        let mult = config.latency_distribution.sample(rand);
        let range = (config.max_message_latency - config.min_message_latency).as_millis() as f64;
        let delay = config.min_message_latency + Duration::from_millis((range * mult) as _);

        std::cmp::min(delay, config.max_message_latency)
    }

    fn latency(&mut self, global: &config::Latency) -> &mut config::Latency {
        self.config.latency.get_or_insert_with(|| global.clone())
    }

    fn message_loss(&mut self, global: &config::MessageLoss) -> &mut config::MessageLoss {
        self.config
            .message_loss
            .get_or_insert_with(|| global.clone())
    }
}
