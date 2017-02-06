use petgraph::graph::NodeIndex;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::sync::mpsc;

use std::collections::hash_map::Entry;

use flow::prelude::*;
use flow::domain::single::NodeDescriptor;

use ops;
use checktable;

#[derive(Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, Debug)]
pub struct Index(usize);

impl From<usize> for Index {
    fn from(i: usize) -> Self {
        Index(i)
    }
}

impl Into<usize> for Index {
    fn into(self) -> usize {
        self.0
    }
}

pub enum ReplayBatch {
    Full(NodeAddress, State),
    Partial(Message),
}

pub enum Control {
    AddNode(NodeDescriptor, Vec<LocalNodeIndex>),
    Ready(LocalNodeIndex, Option<usize>, mpsc::SyncSender<()>),
    ReplayThrough(Vec<NodeAddress>,
                  mpsc::Receiver<ReplayBatch>,
                  Option<mpsc::SyncSender<ReplayBatch>>,
                  mpsc::SyncSender<()>),
    Replay(Vec<NodeAddress>, Option<mpsc::SyncSender<ReplayBatch>>, mpsc::SyncSender<()>),
    PrepareState(LocalNodeIndex, usize),

    /// At the start of a migration, flush pending transactions then notify blender.
    StartMigration(i64, mpsc::SyncSender<()>),
    /// At the end of a migration, send the new timestamp and ingress_from_base counts.
    CompleteMigration(i64, HashMap<NodeIndex, usize>),
}

pub mod single;
pub mod local;

enum BufferedTransaction {
    RemoteTransaction,
    Transaction(NodeIndex, Vec<Message>),
    MigrationStart(mpsc::SyncSender<()>),
    MigrationEnd(HashMap<NodeIndex, usize>),
}

pub struct Domain {
    nodes: DomainNodes,
    state: StateMap,

    /// Map from timestamp to data buffered for that timestamp.
    buffered_transactions: HashMap<i64, BufferedTransaction>,
    /// Number of ingress nodes in the domain that receive updates from each base node. Base nodes
    /// that are only connected by timestamp ingress nodes are not included.
    ingress_from_base: HashMap<NodeIndex, usize>,
    /// Timestamp that the domain has seen all transactions up to.
    ts: i64,

    not_ready: HashSet<LocalNodeIndex>,

    checktable: Arc<Mutex<checktable::CheckTable>>,

    replaying_to: Option<(LocalNodeIndex, Vec<Message>)>,
}

impl Domain {
    pub fn new(nodes: DomainNodes,
               checktable: Arc<Mutex<checktable::CheckTable>>,
               ts: i64)
               -> Self {
        // initially, all nodes are not ready (except for timestamp egress nodes)!
        let not_ready = nodes.iter()
            .filter_map(|n| {
                use flow::node::Type;
                if let Type::TimestampEgress(..) = *n.borrow().inner {
                    return None;
                }

                Some(*n.borrow().addr().as_local())
            })
            .collect();

        Domain {
            nodes: nodes,
            state: StateMap::default(),
            buffered_transactions: HashMap::new(),
            ingress_from_base: HashMap::new(),
            not_ready: not_ready,
            ts: ts,
            checktable: checktable,
            replaying_to: None,
        }
    }

    pub fn dispatch(m: Message,
                    not_ready: &HashSet<LocalNodeIndex>,
                    replaying_to: &mut Option<(LocalNodeIndex, Vec<Message>)>,
                    states: &mut StateMap,
                    nodes: &DomainNodes,
                    enable_output: bool)
                    -> HashMap<NodeAddress, Vec<ops::Record>> {
        let me = m.to;
        let ts = m.ts;
        let mut output_messages = HashMap::new();

        if let Some((ref bufnode, ref mut buffered)) = *replaying_to {
            if bufnode == me.as_local() {
                buffered.push(m);
                return output_messages;
            }
        }
        if !not_ready.is_empty() && not_ready.contains(me.as_local()) {
            return output_messages;
        }

        let mut n = nodes[me.as_local()].borrow_mut();
        let mut u = n.process(m, states, nodes, true);
        drop(n);

        if ts.is_some() {
            // Any message with a timestamp (ie part of a transaction) must flow through the entire
            // graph, even if there are no updates associated with it.
            u = u.or_else(|| Some((Records::default(), ts, None)));
        }

        if u.is_none() {
            // no need to deal with our children if we're not sending them anything
            return output_messages;
        }

        let n = nodes[me.as_local()].borrow();
        for i in 0..n.children.len() {
            // avoid cloning if we can
            let (data, ts, token) = if i == n.children.len() - 1 {
                u.take().unwrap()
            } else {
                u.clone().unwrap()
            };

            if enable_output || !nodes[n.children[i].as_local()].borrow().is_output() {
                let m = Message {
                    from: me,
                    to: n.children[i],
                    data: data,
                    ts: ts,
                    token: token,
                };

                for (k, mut v) in Self::dispatch(m,
                                                 not_ready,
                                                 replaying_to,
                                                 states,
                                                 nodes,
                                                 enable_output) {
                    output_messages.entry(k).or_insert_with(Vec::new).append(&mut v);
                }
            } else {
                let mut data = data;
                match output_messages.entry(n.children[i]) {
                    Entry::Occupied(entry) => {
                        entry.into_mut().append(&mut data);
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(data.into());
                    }
                };
            }
        }

        output_messages
    }

    pub fn transactional_dispatch(&mut self, messages: Vec<Message>) {
        assert!(!messages.is_empty());

        let mut egress_messages = HashMap::new();
        let ts = messages.iter().next().unwrap().ts;

        for m in messages {
            let new_messages = Self::dispatch(m,
                                              &self.not_ready,
                                              &mut self.replaying_to,
                                              &mut self.state,
                                              &self.nodes,
                                              false);

            for (key, mut value) in new_messages {
                egress_messages.entry(key).or_insert_with(Vec::new).append(&mut value);
            }
        }

        for n in self.nodes.iter().filter(|n| n.borrow().is_output()) {
            let data = match egress_messages.entry(n.borrow().addr()) {
                Entry::Occupied(entry) => entry.remove().into(),
                _ => Records::default(),
            };

            let m = Message {
                from: n.borrow().addr(), // TODO: message should be from actual parent, not self.
                to: n.borrow().addr(),
                data: data,
                ts: ts,
                token: None,
            };

            if !self.not_ready.is_empty() && self.not_ready.contains(m.to.as_local()) {
                continue;
            }

            self.nodes[m.to.as_local()]
                .borrow_mut()
                .process(m, &mut self.state, &self.nodes, true);
            assert_eq!(n.borrow().children.len(), 0);
        }
    }

    fn apply_transactions(&mut self) {
        while !self.buffered_transactions.is_empty() {
            let e = {
                // If we don't have anything for this timestamp yet, then stop.
                let entry = match self.buffered_transactions.entry(self.ts + 1) {
                    Entry::Occupied(e) => e,
                    _ => break,
                };

                // If this is a normal transaction and we don't have all the messages for this
                // timestamp, then stop.
                if let BufferedTransaction::Transaction(base, ref messages) = *entry.get() {
                    if messages.len() < self.ingress_from_base[&base] {
                        break;
                    }
                }
                entry.remove()
            };

            match e {
                BufferedTransaction::RemoteTransaction => {}
                BufferedTransaction::Transaction(_, messages) => {
                    self.transactional_dispatch(messages);
                }
                BufferedTransaction::MigrationStart(channel) => {
                    let _ = channel.send(());
                }
                BufferedTransaction::MigrationEnd(ingress_from_base) => {
                    self.ingress_from_base = ingress_from_base;
                }
            }
            self.ts += 1;
        }
    }

    fn buffer_transaction(&mut self, m: Message) {
        let (ts, base) = m.ts.unwrap();

        // Insert message into buffer.
        match *self.buffered_transactions
            .entry(ts)
            .or_insert_with(|| BufferedTransaction::Transaction(base, vec![])) {
            BufferedTransaction::Transaction(_, ref mut messages) => messages.push(m),
            _ => unreachable!(),
        }

        if ts == self.ts + 1 {
            self.apply_transactions();
        }
    }

    pub fn boot(mut self,
                rx: mpsc::Receiver<Message>,
                timestamp_rx: mpsc::Receiver<i64>)
                -> mpsc::SyncSender<Control> {
        use std::thread;

        let (ctx, crx) = mpsc::sync_channel(16);

        let name: usize = self.nodes.iter().next().unwrap().borrow().domain().into();
        thread::Builder::new()
            .name(format!("domain{}", name))
            .spawn(move || {
                let sel = mpsc::Select::new();
                let mut rx_handle = sel.handle(&rx);
                let mut timestamp_rx_handle = sel.handle(&timestamp_rx);
                let mut control_rx_handle = sel.handle(&crx);

                unsafe {
                    rx_handle.add();
                    timestamp_rx_handle.add();
                    control_rx_handle.add();
                }

                loop {
                    let id = sel.wait();
                    if id == control_rx_handle.id() {
                        let control = control_rx_handle.recv();
                        if control.is_err() {
                            return;
                        }
                        self.handle_control(control.unwrap());
                    } else if id == timestamp_rx_handle.id() {
                        let ts = timestamp_rx_handle.recv();
                        if ts.is_err() {
                            return;
                        }
                        let ts = ts.unwrap();

                        let o = BufferedTransaction::RemoteTransaction;
                        let o = self.buffered_transactions.insert(ts, o);
                        assert!(o.is_none());

                        self.apply_transactions();
                    } else if id == rx_handle.id() {
                        let m = rx_handle.recv();
                        if m.is_err() {
                            return;
                        }
                        let mut m = m.unwrap();

                        if let Some((token, send)) = m.token.take() {
                            let ingress = self.nodes[m.to.as_local()].borrow();
                            // TODO: is this the correct node?
                            let base_node =
                                self.nodes[ingress.children[0].as_local()].borrow().index;
                            let result = self.checktable
                                .lock()
                                .unwrap()
                                .claim_timestamp(&token, base_node, &m.data);
                            match result {
                                checktable::TransactionResult::Committed(i) => {
                                    m.ts = Some((i, base_node));
                                    m.token = None;
                                    let _ = send.send(result);
                                }
                                checktable::TransactionResult::Aborted => {
                                    let _ = send.send(result);
                                    continue;
                                }
                            }
                        }

                        match m.ts {
                            None => {
                                Self::dispatch(m,
                                               &self.not_ready,
                                               &mut self.replaying_to,
                                               &mut self.state,
                                               &self.nodes,
                                               true);
                            }
                            Some(_) => {
                                self.buffer_transaction(m);
                            }
                        }
                    }
                }
            })
            .unwrap();

        ctx
    }

    fn replay_done(&mut self, node: LocalNodeIndex) {
        self.not_ready.remove(&node);
        if let Some((target, buffered)) = self.replaying_to.take() {
            assert_eq!(target, node);
            assert!(buffered.is_empty());
        }
    }

    fn handle_control(&mut self, c: Control) {
        use itertools::Itertools;
        match c {
            Control::AddNode(n, parents) => {
                use std::cell;
                let addr = *n.addr().as_local();
                self.not_ready.insert(addr);

                for p in parents {
                    self.nodes.get_mut(&p).unwrap().borrow_mut().children.push(n.addr());
                }
                self.nodes.insert(addr, cell::RefCell::new(n));
            }
            Control::Ready(ni, index_on, ack) => {
                if let Some(index_on) = index_on {
                    let mut s = {
                        let n = self.nodes[&ni].borrow();
                        if n.is_internal() && n.is_base() {
                            State::base()
                        } else {
                            State::default()
                        }
                    };
                    s.set_pkey(index_on);
                    self.state.insert(ni, s);
                } else {
                    // NOTE: just because index_on is None does *not* mean we're not materialized
                }

                // swap replayed reader nodes to expose new state
                {
                    use flow::node::Type;
                    let mut n = self.nodes[&ni].borrow_mut();
                    if let Type::Reader(ref mut w, _) = *n.inner {
                        if let Some(ref mut state) = *w {
                            state.swap();
                        }
                    }
                }

                self.replay_done(ni);
                drop(ack);
            }
            Control::PrepareState(ni, on) => {
                let mut state = State::default();
                state.set_pkey(on);
                self.state.insert(ni, state);
            }
            Control::Replay(nodes, mut tx, ack) => {
                // let coordinator know that we've entered replay loop
                ack.send(()).unwrap();

                // check for stupidity
                assert!(!nodes.is_empty());

                // we know that nodes[0] is materialized, as the migration coordinator picks path
                // that originate with materialized nodes. if this weren't the case, we wouldn't be
                // able to do the replay, and the entire migration would fail.
                //
                // we clone the entire state so that we can continue to occasionally process
                // incoming updates to the domain without disturbing the state that is being
                // replayed.
                let state: State = self.state
                    .get(nodes[0].as_local())
                    .expect("migration replay path started with non-materialized node")
                    .clone();

                if nodes.len() == 1 {
                    // now, we can just send our entire state in one go, rather than chunk it. this
                    // will be much faster than iterating over the map one-by-one and cloning each
                    // record. furthermore, it allows the receiver to simply replace their current
                    // empty state with this state if it is not passing thorugh other nodes.
                    if let Some(tx) = tx {
                        tx.send(ReplayBatch::Full(nodes[0], state)).unwrap();
                    } else {
                        // replaying a single node has no purpose if there isn't someone we're
                        // sending to.
                        unreachable!()
                    }
                    return;
                }

                // TODO: in the special case where nodes.len() == 2 and tx.is_none(), and we
                // literally just need a copy of the state, we could early-terminate here.

                // since we must have more than one node, this is safe
                let init_to = nodes[1];

                if tx.is_none() {
                    // the sink node is in this domain. make sure we buffer any updates that get
                    // propagated to it during the migration (as they logically follow the state
                    // snapshot that is being replayed to it).
                    self.replaying_to = Some((*nodes.last().as_ref().unwrap().as_local(),
                                              Vec::new()));
                }

                // process all records in state to completion within domain
                // and then forward on tx (if there is one)
                'chunks: for chunk in state.into_iter()
                    .flat_map(|(_, rs)| rs)
                    .chunks(1000)
                    .into_iter() {
                    use std::iter::FromIterator;
                    let chunk = Records::from_iter(chunk.into_iter());
                    let mut m = Message {
                        from: nodes[0],
                        to: init_to,
                        data: chunk,
                        ts: None,
                        token: None,
                    };

                    // forward the current chunk through all local nodes
                    for (i, ni) in nodes.iter().enumerate().skip(1) {
                        // process the current chunk in this node
                        let mut n = self.nodes[ni.as_local()].borrow_mut();
                        assert!(ni != &nodes[0]);
                        let u = n.process(m, &mut self.state, &self.nodes, false);
                        drop(n);

                        if u.is_none() {
                            continue 'chunks;
                        }

                        m = Message {
                            from: *ni,
                            to: *ni,
                            data: u.unwrap().0,
                            ts: None,
                            token: None,
                        };

                        if i != nodes.len() - 1 {
                            m.to = nodes[i + 1];
                        } else {
                            // to is overwritten by receiving domain. from doesn't need to be set
                            // to the egress, because the ingress ignores it. setting it to this
                            // node is basically just as correct.
                        }
                    }

                    if let Some(tx) = tx.as_mut() {
                        tx.send(ReplayBatch::Partial(m)).unwrap();
                    }
                }

                if tx.is_none() {
                    // we must mark the node as ready immediately, otherwise it might miss updates
                    // that follow the replay, but precede the ready.
                    self.replay_done(*nodes.last().unwrap().as_local());
                }
            }
            Control::ReplayThrough(nodes, rx, mut tx, ack) => {
                // let coordinator know that we've entered replay loop
                ack.send(()).unwrap();

                // a couple of shortcuts first...
                // if nodes.len() == 1, we know we're an ingress node, and we can just stuff the
                // state directly into it. we *also* know that that ingress is the node whose state
                // is being rebuilt.
                if nodes.len() == 1 {
                    assert!(self.nodes[nodes[0].as_local()].borrow().is_ingress());
                    assert!(tx.is_none());
                    for batch in rx {
                        match batch {
                            ReplayBatch::Full(_, state) => {
                                // oh boy, we're in luck! we just sent the full state we need for
                                // this node. no need to process or anything, just move in the
                                // state and we're done.
                                // TODO: fall back to regular replay here
                                assert_eq!(self.state[nodes[0].as_local()].get_pkey(),
                                           state.get_pkey());
                                self.state.insert(*nodes[0].as_local(), state);
                                break;
                            }
                            ReplayBatch::Partial(m) => {
                                let state = self.state.get_mut(nodes[0].as_local()).unwrap();
                                for r in m.data.into_iter() {
                                    match r {
                                        ops::Record::Positive(r) => state.insert(r),
                                        ops::Record::Negative(ref r) => state.remove(r),
                                    }
                                }
                            }
                        }
                    }
                    self.replay_done(*nodes[0].as_local());
                    return;
                }

                let rx = BatchedIterator::new(rx, nodes[0]);

                if tx.is_none() {
                    // the sink node is in this domain. make sure we buffer any updates that get
                    // propagated to it during the migration (as they logically follow the state
                    // snapshot that is being replayed to it).
                    self.replaying_to = Some((*nodes.last().as_ref().unwrap().as_local(),
                                              Vec::new()));
                }

                // process all records in state to completion within domain
                // and then forward on tx (if there is one)
                'replay: for mut m in rx {
                    // forward the current message through all local nodes
                    for (i, ni) in nodes.iter().enumerate() {
                        // process the current message in this node
                        let mut n = self.nodes[ni.as_local()].borrow_mut();
                        let u = n.process(m, &mut self.state, &self.nodes, false);
                        drop(n);

                        if u.is_none() {
                            continue 'replay;
                        }

                        m = Message {
                            from: *ni,
                            to: *ni,
                            data: u.unwrap().0,
                            ts: None,
                            token: None,
                        };

                        if i != nodes.len() - 1 {
                            m.to = nodes[i + 1];
                        } else {
                            // to is overwritten by receiving domain. from doesn't need to be set
                            // to the egress, because the ingress ignores it. setting it to this
                            // node is basically just as correct.
                        }
                    }

                    if let Some(tx) = tx.as_mut() {
                        tx.send(ReplayBatch::Partial(m)).unwrap();
                    }
                }

                if tx.is_none() {
                    // we must mark the node as ready immediately, otherwise it might miss updates
                    // that follow the replay, but precede the ready.
                    self.replay_done(*nodes.last().unwrap().as_local());
                }
            }
            Control::StartMigration(ts, channel) => {
                let o = self.buffered_transactions
                    .insert(ts, BufferedTransaction::MigrationStart(channel));
                assert!(o.is_none());

                if ts == self.ts + 1 {
                    self.apply_transactions();
                }
            }
            Control::CompleteMigration(ts, ingress_from_base) => {
                let o = self.buffered_transactions
                    .insert(ts, BufferedTransaction::MigrationEnd(ingress_from_base));
                assert!(o.is_none());
                assert_eq!(ts, self.ts + 1);
                self.apply_transactions();
            }
        }
    }
}

use std::collections::hash_map;
struct BatchedIterator {
    rx: mpsc::IntoIter<ReplayBatch>,
    state_iter: Option<hash_map::IntoIter<DataType, Vec<Arc<Vec<DataType>>>>>,
    to: NodeAddress,
    from: Option<NodeAddress>,
}

impl BatchedIterator {
    fn new(rx: mpsc::Receiver<ReplayBatch>, to: NodeAddress) -> Self {
        BatchedIterator {
            rx: rx.into_iter(),
            state_iter: None,
            to: to,
            from: None,
        }
    }
}

impl Iterator for BatchedIterator {
    type Item = Message;
    fn next(&mut self) -> Option<Self::Item> {
        use itertools::Itertools;
        if let Some(ref mut state_iter) = self.state_iter {
            let from = self.from.unwrap();
            let to = self.to;
            state_iter.flat_map(|(_, rs)| rs)
                .chunks(1000)
                .into_iter()
                .map(|chunk| {
                    use std::iter::FromIterator;
                    Message {
                        from: from,
                        to: to,
                        data: FromIterator::from_iter(chunk.into_iter()),
                        ts: None,
                        token: None,
                    }
                })
                .next()
        } else {
            match self.rx.next() {
                None => None,
                Some(ReplayBatch::Partial(m)) => Some(m),
                Some(ReplayBatch::Full(from, state)) => {
                    self.from = Some(from);
                    self.state_iter = Some(state.into_iter());
                    self.next()
                }
            }
        }
    }
}
