//! K-RPC implementatioStoreQueryMetdatan

mod closest_nodes;
mod config;
mod info;
mod query;
mod socket;

use std::collections::HashMap;
use std::net::{SocketAddr, SocketAddrV4, ToSocketAddrs};
use std::num::NonZeroUsize;
use std::rc::Rc;
use std::time::{Duration, Instant};

use lru::LruCache;
use tracing::{debug, error, info};

use crate::common::{
    validate_immutable, ErrorSpecific, FindNodeRequestArguments, GetImmutableResponseArguments,
    GetMutableResponseArguments, GetPeersResponseArguments, GetValueRequestArguments, Id, Message,
    MessageType, MutableItem, NoMoreRecentValueResponseArguments, NoValuesResponseArguments, Node,
    PutRequestSpecific, RequestSpecific, RequestTypeSpecific, ResponseSpecific, RoutingTable,
    MAX_BUCKET_SIZE_K,
};
use crate::server::{DefaultServer, Server};

use query::{IterativeQuery, PutQuery};
use socket::KrpcSocket;

pub use crate::common::messages;
pub use closest_nodes::ClosestNodes;
pub use config::Config;
pub use info::Info;
pub use query::PutError;
pub use socket::DEFAULT_PORT;
pub use socket::DEFAULT_REQUEST_TIMEOUT;

use self::messages::{
    AnnouncePeerRequestArguments, GetPeersRequestArguments, PutImmutableRequestArguments,
    PutMutableRequestArguments, PutRequest,
};

pub const DEFAULT_BOOTSTRAP_NODES: [&str; 4] = [
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "dht.libtorrent.org:25401",
    "relay.pkarr.org:6881",
];

const REFRESH_TABLE_INTERVAL: Duration = Duration::from_secs(15 * 60);
const PING_TABLE_INTERVAL: Duration = Duration::from_secs(5 * 60);

const MAX_CACHED_ITERATIVE_QUERIES: usize = 1000;

#[derive(Debug)]
/// Internal Rpc called in the Dht thread loop, useful to create your own actor setup.
pub struct Rpc {
    // Options
    bootstrap: Vec<SocketAddr>,

    socket: KrpcSocket,

    // Routing
    /// Closest nodes to this node
    routing_table: RoutingTable,
    /// Last time we refreshed the routing table with a find_node query.
    last_table_refresh: Instant,
    /// Last time we pinged nodes in the routing table.
    last_table_ping: Instant,
    /// Closest responding nodes to specific target
    ///
    /// as well as the:
    /// 1. dht size estimate based on closest claimed nodes,
    /// 2. dht size estimate based on closest responding nodes.
    /// 3. number of subnets with unique 6 bits prefix in ipv4
    cached_iterative_queries: LruCache<Id, CachedIterativeQuery>,

    // Active IterativeQueries
    iterative_queries: HashMap<Id, IterativeQuery>,
    /// Put queries are special, since they have to wait for a corresponing
    /// get query to finish, update the closest_nodes, then `query_all` these.
    put_queries: HashMap<Id, PutQuery>,

    /// Sum of Dht size estimates from closest nodes from get queries.
    dht_size_estimates_sum: f64,

    /// Sum of Dht size estimates from closest _responding_ nodes from get queries.
    responders_based_dht_size_estimates_sum: f64,
    responders_based_dht_size_estimates_count: usize,

    /// Sum of the number of subnets with 6 bits prefix in the closest nodes ipv4
    subnets_sum: usize,

    server: Box<dyn Server>,

    public_address: Option<SocketAddrV4>,
    firewalled: bool,
}

impl Rpc {
    /// Create a new Rpc
    pub fn new(config: Config) -> Result<Self, std::io::Error> {
        let id = if let Some(ip) = config.public_ip {
            Id::from_ip(ip.into())
        } else {
            Id::random()
        };

        let socket = KrpcSocket::new(&config)?;

        let bootstrap = config
            .bootstrap
            .to_owned()
            .iter()
            .flat_map(|s| s.to_socket_addrs().map(|addrs| addrs.collect::<Vec<_>>()))
            .flatten()
            .collect::<Vec<_>>();

        Ok(Rpc {
            bootstrap,
            socket,

            routing_table: RoutingTable::new().with_id(id),
            iterative_queries: HashMap::new(),
            put_queries: HashMap::new(),

            cached_iterative_queries: LruCache::new(
                NonZeroUsize::new(MAX_CACHED_ITERATIVE_QUERIES)
                    .expect("MAX_CACHED_BUCKETS is NonZeroUsize"),
            ),

            last_table_refresh: Instant::now(),
            last_table_ping: Instant::now(),

            dht_size_estimates_sum: 0.0,
            responders_based_dht_size_estimates_count: 0,

            // Don't store to too many nodes just because you are in a cold start.
            responders_based_dht_size_estimates_sum: 1_000_000.0,
            subnets_sum: 20,

            server: config.server.unwrap_or(Box::new(DefaultServer::default())),

            public_address: None,
            firewalled: true,
        })
    }

    // === Getters ===

    /// Returns the node's Id
    pub fn id(&self) -> &Id {
        self.routing_table.id()
    }

    /// Returns the address the server is listening to.
    #[inline]
    pub fn local_addr(&self) -> SocketAddrV4 {
        self.socket.local_addr()
    }

    /// Returns the best guess for this node's Public addresss.
    ///
    /// If [Config::public_ip] was set, this is what will be returned
    /// (plus the local port), otherwise it will rely on consensus from
    /// responding nodes voting on our public IP and port.
    pub fn public_address(&self) -> Option<SocketAddrV4> {
        self.public_address
    }

    /// Returns `true` if we can't confirm that [Self::public_address] is publicly addressable.
    ///
    /// If this node is firewalled, it won't switch to server mode if it is in adaptive mode,
    /// but if [Config::server_mode] was set to true, then whether or not this node is firewalled
    /// won't matter.
    pub fn firewalled(&self) -> bool {
        self.firewalled
    }

    /// Returns whether or not this node is running in server mode.
    pub fn server_mode(&self) -> bool {
        self.socket.server_mode
    }

    pub fn routing_table(&self) -> &RoutingTable {
        &self.routing_table
    }

    /// Returns:
    ///  1. Normal Dht size estimate based on all closer `nodes` in query responses.
    ///  2. Standard deviaiton as a function of the number of samples used in this estimate.
    ///
    /// [Read more](https://github.com/pubky/mainline/blob/main/docs/dht_size_estimate.md)
    pub fn dht_size_estimate(&self) -> (usize, f64) {
        let normal =
            self.dht_size_estimates_sum as usize / self.cached_iterative_queries.len().max(1);

        // See https://github.com/pubky/mainline/blob/main/docs/standard-deviation-vs-lookups.png
        let std_dev = 0.281 * (self.cached_iterative_queries.len() as f64).powf(-0.529);

        (normal, std_dev)
    }

    /// Returns a thread safe and lightweight summary of this node's
    /// information and statistics.
    pub fn info(&self) -> Info {
        Info::from(self)
    }

    // === Public Methods ===

    /// Advance the inflight queries, receive incoming requests,
    /// maintain the routing table, and everything else that needs
    /// to happen at every tick.
    pub fn tick(&mut self) -> RpcTickReport {
        let mut done_get_queries = Vec::with_capacity(self.iterative_queries.len());
        let mut done_put_queries = Vec::with_capacity(self.put_queries.len());
        let mut done_find_node_queries = Vec::with_capacity(self.put_queries.len());

        // === Tick Queries ===

        for (id, query) in self.put_queries.iter_mut() {
            match query.tick(&mut self.socket) {
                Ok(done) => {
                    if done {
                        done_put_queries.push((*id, None));
                    }
                }
                Err(error) => done_put_queries.push((*id, Some(error))),
            };
        }

        let self_id = *self.id();
        let table_size = self.routing_table.size();

        for (id, query) in self.iterative_queries.iter_mut() {
            let is_done = query.tick(&mut self.socket);

            if is_done {
                if let RequestTypeSpecific::FindNode(_) = query.request.request_type {
                    let closest_nodes = query
                        .closest()
                        .nodes()
                        .iter()
                        .take(MAX_BUCKET_SIZE_K)
                        .map(|n| n.as_ref().clone())
                        .collect::<Vec<_>>();

                    done_find_node_queries.push((*id, closest_nodes));

                    if *id == self_id {
                        if table_size == 0 {
                            error!("Could not bootstrap the routing table");
                        } else {
                            debug!(?self_id, table_size, "Populated the routing table");
                        }
                    };
                } else {
                    done_get_queries.push(*id);
                }
            };
        }

        // === Cleanup done queries ===

        // Has to happen _before_ `self.socket.recv_from()`.
        for id in &done_get_queries {
            if let Some(query) = self.iterative_queries.remove(id) {
                let closest_responding_nodes = self.handle_iterative_query(&query);

                self.responders_based_dht_size_estimates_count += 1;

                if let Some(put_query) = self.put_queries.get_mut(id) {
                    if let Err(error) = put_query.start(&mut self.socket, closest_responding_nodes)
                    {
                        done_put_queries.push((*id, Some(error)))
                    }
                }
            };
        }

        for (id, _) in &done_put_queries {
            self.put_queries.remove(id);
        }

        for (id, _) in &done_find_node_queries {
            if let Some(query) = self.iterative_queries.remove(id) {
                self.check_address_votes_from_iterative_query(&query);
                self.handle_iterative_query(&query);
            }
        }

        // === Periodic node maintainance ===
        self.periodic_node_maintainance();

        // Handle new incoming message
        let query_response =
            self.socket
                .recv_from()
                .and_then(|(message, from)| match message.message_type {
                    MessageType::Request(request_specific) => {
                        self.handle_request(from, message.transaction_id, request_specific);

                        None
                    }
                    _ => self.handle_response(from, message),
                });

        RpcTickReport {
            done_get_queries,
            done_put_queries,
            done_find_node_queries,
            query_response,
        }
    }

    /// Send a request to the given address and return the transaction_id
    pub fn request(&mut self, address: SocketAddr, request: RequestSpecific) -> u16 {
        self.socket.request(address, request)
    }

    /// Send a response to the given address.
    pub fn response(
        &mut self,
        address: SocketAddr,
        transaction_id: u16,
        response: ResponseSpecific,
    ) {
        self.socket.response(address, transaction_id, response)
    }

    /// Send an error to the given address.
    pub fn error(&mut self, address: SocketAddr, transaction_id: u16, error: ErrorSpecific) {
        self.socket.error(address, transaction_id, error)
    }

    /// Store a value in the closest nodes, optionally trigger a lookup query if
    /// the cached closest_nodes aren't fresh enough.
    ///
    /// - `request`: the put request.
    pub fn put(&mut self, request: PutRequestSpecific) -> Result<(), PutError> {
        let target = match request {
            PutRequestSpecific::AnnouncePeer(AnnouncePeerRequestArguments {
                info_hash, ..
            }) => info_hash,
            PutRequestSpecific::PutMutable(PutMutableRequestArguments { target, .. }) => target,
            PutRequestSpecific::PutImmutable(PutImmutableRequestArguments { target, .. }) => target,
        };

        if self.put_queries.contains_key(&target) {
            debug!(?target, "Put query for the same target is already inflight");

            return Err(PutError::PutQueryIsInflight(target));
        }

        let mut query = PutQuery::new(target, request.clone());

        if let Some(closest_nodes) = self
            .cached_iterative_queries
            .get(&target)
            .map(|cached| cached.closest_responding_nodes.clone())
            .filter(|closest_nodes| {
                !closest_nodes.is_empty() && closest_nodes.iter().any(|n| n.valid_token())
            })
        {
            query.start(&mut self.socket, closest_nodes)?
        } else {
            let salt = match request {
                PutRequestSpecific::PutMutable(args) => args.salt,
                _ => None,
            };

            self.get(
                RequestTypeSpecific::GetValue(GetValueRequestArguments {
                    target,
                    seq: None,
                    salt: salt.map(|s| s.into()),
                }),
                None,
            );
        };

        self.put_queries.insert(target, query);

        Ok(())
    }

    /// Send a message to closer and closer nodes until we can't find any more nodes.
    ///
    /// Queries take few seconds to fully traverse the network, once it is done, it will be removed from
    /// self.iterative_queries. But until then, calling [Rpc::get] multiple times, will just return the list
    /// of responses seen so far.
    ///
    /// Subsequent responses can be obtained from the [RpcTickReport::query_response] you get after calling [Rpc::tick].
    ///
    /// Effectively, we are caching responses and backing off the network for the duration it takes
    /// to traverse it.
    ///
    /// - `request` [RequestTypeSpecific], except [RequestTypeSpecific::Ping] and
    ///     [RequestTypeSpecific::Put] which will be ignored.
    /// - `extra_nodes` option allows the query to visit specific nodes, that won't necessesarily be visited
    ///     through the query otherwise.
    pub fn get(
        &mut self,
        request: RequestTypeSpecific,
        extra_nodes: Option<Vec<SocketAddr>>,
    ) -> Option<Vec<Response>> {
        let target = match request {
            RequestTypeSpecific::FindNode(FindNodeRequestArguments { target }) => target,
            RequestTypeSpecific::GetPeers(GetPeersRequestArguments { info_hash, .. }) => info_hash,
            RequestTypeSpecific::GetValue(GetValueRequestArguments { target, .. }) => target,
            _ => {
                return None;
            }
        };

        // If query is still active, no need to create a new one.
        if let Some(query) = self.iterative_queries.get(&target) {
            return Some(query.responses().to_vec());
        }

        let node_id = self.routing_table.id();

        if target == *node_id {
            debug!(?node_id, "Bootstraping the routing table");
        }

        let mut query = IterativeQuery::new(
            target,
            RequestSpecific {
                requester_id: *node_id,
                request_type: request,
            },
        );

        // Seed the query either with the closest nodes from the routing table, or the
        // bootstrapping nodes if the closest nodes are not enough.

        let routing_table_closest = self.routing_table.closest_secure(
            target,
            self.responders_based_dht_size_estimate(),
            self.average_subnets(),
        );

        // If we don't have enough or any closest nodes, call the bootstraping nodes.
        if routing_table_closest.is_empty() || routing_table_closest.len() < self.bootstrap.len() {
            for bootstrapping_node in self.bootstrap.clone() {
                query.visit(&mut self.socket, bootstrapping_node);
            }
        }

        if let Some(extra_nodes) = extra_nodes {
            for extra_node in extra_nodes {
                query.visit(&mut self.socket, extra_node)
            }
        }

        // Seed this query with the closest nodes we know about.
        for node in routing_table_closest {
            query.add_candidate(node)
        }

        if let Some(CachedIterativeQuery {
            closest_responding_nodes,
            ..
        }) = self.cached_iterative_queries.get(&target)
        {
            for node in closest_responding_nodes {
                query.add_candidate(node.clone())
            }
        }

        // After adding the nodes, we need to start the query.
        query.start(&mut self.socket);

        self.iterative_queries.insert(target, query);

        None
    }

    // === Private Methods ===

    fn handle_request(
        &mut self,
        from: SocketAddr,
        transaction_id: u16,
        request_specific: RequestSpecific,
    ) {
        let is_ping = matches!(request_specific.request_type, RequestTypeSpecific::Ping);

        if self.server_mode() {
            let server = &mut self.server;

            match server.handle_request(&self.routing_table, from, request_specific) {
                (MessageType::Error(error), _) => {
                    self.error(from, transaction_id, error);
                }
                (MessageType::Response(response), _) => {
                    self.response(from, transaction_id, response);
                }
                (MessageType::Request(request), extra_nodes) => {
                    debug!(
                        ?request,
                        "Sending a request (from Rpc::server) after handling a request!"
                    );

                    match request {
                        RequestSpecific {
                            request_type: RequestTypeSpecific::Ping,
                            ..
                        } => {
                            // Ignoring ping.
                        }
                        RequestSpecific {
                            request_type:
                                RequestTypeSpecific::Put(PutRequest {
                                    put_request_type, ..
                                }),
                            ..
                        } => {
                            tracing::trace!("custom server returned a PUT request, sending it.");
                            let _ = self.put(put_request_type);
                        }
                        RequestSpecific { request_type, .. } => {
                            tracing::trace!("custom server returned a GET request, sending it.");
                            let _ = self.get(request_type, extra_nodes);
                        }
                    }
                }
            };
        }

        if let Some(our_adress) = self.public_address {
            if let SocketAddr::V4(from) = from {
                if from == our_adress && is_ping {
                    self.firewalled = false;

                    let ipv4 = our_adress.ip();

                    // Restarting our routing table with new secure Id if necessary.
                    if !self.id().is_valid_for_ipv4(*ipv4) {
                        let new_id = Id::from_ipv4(*ipv4);

                        info!(
                            "Our current id {} is not valid for adrsess {}. Using new id {}",
                            self.id(),
                            our_adress,
                            new_id
                        );

                        self.get(
                            RequestTypeSpecific::FindNode(FindNodeRequestArguments {
                                target: new_id,
                            }),
                            None,
                        );

                        self.routing_table = RoutingTable::new().with_id(new_id);
                    }
                }
            }
        }
    }

    fn handle_response(&mut self, from: SocketAddr, message: Message) -> Option<(Id, Response)> {
        // If someone claims to be readonly, then let's not store anything even if they respond.
        if message.read_only {
            return None;
        };

        // If the response looks like a Ping response, check StoreQueries for the transaction_id.
        if let Some(query) = self
            .put_queries
            .values_mut()
            .find(|query| query.inflight(message.transaction_id))
        {
            match message.message_type {
                MessageType::Response(ResponseSpecific::Ping(_)) => {
                    // Mark storage at that node as a success.
                    query.success();
                }
                MessageType::Error(error) => query.error(error),
                _ => {}
            };

            return None;
        }

        let mut should_add_node = false;
        let author_id = message.get_author_id();

        // Get corresponing query for message.transaction_id
        if let Some(query) = self
            .iterative_queries
            .values_mut()
            .find(|query| query.inflight(message.transaction_id))
        {
            // KrpcSocket would not give us a response from the wrong address for the transaction_id
            should_add_node = true;

            if let Some(nodes) = message.get_closer_nodes() {
                for node in nodes {
                    query.add_candidate(node.clone());
                }
            }

            if let Some((responder_id, token)) = message.get_token() {
                query.add_responding_node(
                    Node::new(responder_id, from)
                        .with_token(token.clone())
                        .into(),
                );
            }

            if let Some(proposed_ip) = message.requester_ip {
                query.add_address_vote(proposed_ip);
            }

            let target = query.target();

            match message.message_type {
                MessageType::Response(ResponseSpecific::GetPeers(GetPeersResponseArguments {
                    values,
                    ..
                })) => {
                    let response = Response::Peers(values);
                    query.response(from, response.clone());

                    return Some((target, response));
                }
                MessageType::Response(ResponseSpecific::GetImmutable(
                    GetImmutableResponseArguments {
                        v, responder_id, ..
                    },
                )) => {
                    if validate_immutable(&v, query.target()) {
                        let response = Response::Immutable(v.into());
                        query.response(from, response.clone());

                        return Some((target, response));
                    }

                    let target = query.target();
                    debug!(?v, ?target, ?responder_id, ?from, from_version = ?message.version, "Invalid immutable value");
                }
                MessageType::Response(ResponseSpecific::GetMutable(
                    GetMutableResponseArguments {
                        v,
                        seq,
                        sig,
                        k,
                        responder_id,
                        ..
                    },
                )) => {
                    let salt = match query.request.request_type.clone() {
                        RequestTypeSpecific::GetValue(args) => args.salt,
                        _ => None,
                    };
                    let target = query.target();

                    match MutableItem::from_dht_message(
                        query.target(),
                        &k,
                        v.into(),
                        seq,
                        &sig,
                        salt.as_deref(),
                        None,
                    ) {
                        Ok(item) => {
                            let response = Response::Mutable(item);
                            query.response(from, response.clone());

                            return Some((target, response));
                        }
                        Err(error) => {
                            debug!(
                                ?error,
                                ?from,
                                ?responder_id,
                                from_version = ?message.version,
                                "Invalid mutable record"
                            );
                        }
                    }
                }
                MessageType::Response(ResponseSpecific::NoMoreRecentValue(
                    NoMoreRecentValueResponseArguments {
                        seq, responder_id, ..
                    },
                )) => {
                    debug!(
                        target= ?query.target(),
                        salt= ?match query.request.request_type.clone() {
                            RequestTypeSpecific::GetValue(args) => args.salt,
                            _ => None,
                        },
                        ?seq,
                        ?from,
                        ?responder_id,
                        from_version = ?message.version,
                        "No more recent value"
                    );
                }
                MessageType::Response(ResponseSpecific::NoValues(NoValuesResponseArguments {
                    responder_id,
                    ..
                })) => {
                    debug!(
                        target= ?query.target(),
                        salt= ?match query.request.request_type.clone() {
                            RequestTypeSpecific::GetValue(args) => args.salt,
                            _ => None,
                        },
                        ?from,
                        ?responder_id,
                        from_version = ?message.version,
                        "No values"
                    );
                }
                MessageType::Error(error) => {
                    debug!(?error, from_version = ?message.version, "Get query got error response");
                }
                // Ping response is already handled in add_node()
                // FindNode response is already handled in query.add_candidate()
                // Requests are handled elsewhere
                MessageType::Response(ResponseSpecific::Ping(_))
                | MessageType::Response(ResponseSpecific::FindNode(_))
                | MessageType::Request(_) => {}
            };
        };

        if should_add_node {
            // Add a node to our routing table on any expected incoming response.

            if let Some(id) = author_id {
                self.routing_table.add(Node::new(id, from));
            }
        }

        None
    }

    fn periodic_node_maintainance(&mut self) {
        // Bootstrap if necessary
        if self.routing_table.is_empty() {
            self.populate();
        }

        // Every 15 minutes refresh the routing table.
        if self.last_table_refresh.elapsed() > REFRESH_TABLE_INTERVAL {
            self.last_table_refresh = Instant::now();

            if !self.server_mode() && !self.firewalled() {
                info!("Adaptive mode: have been running long enough (not firewalled), switching to server mode");

                self.socket.server_mode = true;
            }

            self.populate();
        }

        if self.last_table_ping.elapsed() > PING_TABLE_INTERVAL {
            self.last_table_ping = Instant::now();

            for node in self.routing_table.to_vec() {
                if node.is_stale() {
                    self.routing_table.remove(node.id);
                } else if node.should_ping() {
                    self.ping(node.address);
                }
            }
        }
    }

    /// Ping bootstrap nodes, add them to the routing table with closest query.
    fn populate(&mut self) {
        self.get(
            RequestTypeSpecific::FindNode(FindNodeRequestArguments { target: *self.id() }),
            None,
        );
    }

    fn ping(&mut self, address: SocketAddr) {
        self.socket.request(
            address,
            RequestSpecific {
                requester_id: *self.id(),
                request_type: RequestTypeSpecific::Ping,
            },
        );
    }

    fn handle_iterative_query(&mut self, query: &IterativeQuery) -> Vec<Rc<Node>> {
        self.check_address_votes_from_iterative_query(query);
        self.cache_iterative_query(query)
    }

    fn check_address_votes_from_iterative_query(&mut self, query: &IterativeQuery) {
        if let Some(new_address) = query.best_address() {
            if self.public_address.is_none()
                || new_address
                    != self
                        .public_address
                        .expect("self.public_address is not None")
            {
                debug!(
                    ?new_address,
                    "Query responses suggest a different public_address, trying to confirm.."
                );

                self.firewalled = true;
                self.ping(new_address.into());
            }

            self.public_address = Some(new_address)
        }
    }

    fn cache_iterative_query(&mut self, query: &IterativeQuery) -> Vec<Rc<Node>> {
        if self.cached_iterative_queries.len() >= MAX_CACHED_ITERATIVE_QUERIES {
            // Remove least recent closest_nodes
            if let Some((
                _,
                CachedIterativeQuery {
                    dht_size_estimate,
                    responders_dht_size_estimate,
                    subnets,
                    is_find_node,
                    ..
                },
            )) = self.cached_iterative_queries.pop_lru()
            {
                self.dht_size_estimates_sum -= dht_size_estimate;
                self.responders_based_dht_size_estimates_sum -= responders_dht_size_estimate;
                self.subnets_sum -= subnets as usize;

                if !is_find_node {
                    self.responders_based_dht_size_estimates_count -= 1;
                }
            };
        }

        let closest = query.closest();
        let responders = query.responders();

        let dht_size_estimate = closest.dht_size_estimate();
        let responders_dht_size_estimate = responders.dht_size_estimate();
        let subnets_count = closest.subnets_count();

        self.dht_size_estimates_sum += dht_size_estimate;
        self.responders_based_dht_size_estimates_sum += responders_dht_size_estimate;
        self.subnets_sum += subnets_count as usize;

        let closest_responding_nodes = responders
            .take_until_secure(
                self.responders_based_dht_size_estimate(),
                self.average_subnets(),
            )
            .to_vec();

        self.cached_iterative_queries.put(
            query.target(),
            CachedIterativeQuery {
                closest_responding_nodes: closest_responding_nodes.clone(),
                dht_size_estimate,
                responders_dht_size_estimate,
                subnets: subnets_count,

                is_find_node: matches!(
                    query.request.request_type,
                    RequestTypeSpecific::FindNode(_)
                ),
            },
        );

        closest_responding_nodes
    }

    fn responders_based_dht_size_estimate(&self) -> usize {
        self.responders_based_dht_size_estimates_sum as usize
            / self.responders_based_dht_size_estimates_count.max(1)
    }

    fn average_subnets(&self) -> usize {
        self.subnets_sum / self.cached_iterative_queries.len().max(1)
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        debug!("Dropped Mainline::Rpc");
    }
}

struct CachedIterativeQuery {
    closest_responding_nodes: Vec<Rc<Node>>,
    dht_size_estimate: f64,
    responders_dht_size_estimate: f64,
    subnets: u8,

    /// Keeping track of find_node queries, because they shouldn't
    /// be counted in `responders_based_dht_size_estimates_count`
    is_find_node: bool,
}

/// State change after a call to [Rpc::tick], including
/// done PUT, GET, and FIND_NODE queries, as well as any
/// incoming value response for any GET query.
#[derive(Debug, Clone)]
pub struct RpcTickReport {
    /// All the [Id]s of the done [Rpc::get] queries.
    pub done_get_queries: Vec<Id>,
    /// All the [Id]s of the done [Rpc::put] queries,
    /// and optional [PutError] if the query failed.
    pub done_put_queries: Vec<(Id, Option<PutError>)>,
    pub done_find_node_queries: Vec<(Id, Vec<Node>)>,
    /// Received GET query response.
    pub query_response: Option<(Id, Response)>,
}

#[derive(Debug, Clone)]
pub enum Response {
    Peers(Vec<SocketAddr>),
    Immutable(Box<[u8]>),
    Mutable(MutableItem),
}
