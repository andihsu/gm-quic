use std::{
    io, mem,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use qbase::{cid::ConnectionId, error::Error, frame::ConnectionCloseFrame, net::route::Pathway};
use qinterface::queue::RcvdPacketQueue;
use tokio::time::Instant;

use crate::{ArcLocalCids, Components, path::ArcPathContexts};

/// Keep a few states to support sending a packet with ccf.
///
/// when it is dropped all paths will be destroyed
pub struct Terminator {
    last_recv_time: Mutex<Instant>,
    rcvd_packets: AtomicUsize,
    scid: Option<ConnectionId>,
    dcid: Option<ConnectionId>,
    ccf: ConnectionCloseFrame,
    paths: ArcPathContexts,
}

impl Drop for Terminator {
    fn drop(&mut self) {
        self.paths.clear();
    }
}

impl Terminator {
    pub fn new(ccf: ConnectionCloseFrame, components: &Components) -> Self {
        Self {
            last_recv_time: Mutex::new(Instant::now()),
            rcvd_packets: AtomicUsize::new(0),
            scid: components.cid_registry.local.initial_scid(),
            dcid: components.cid_registry.remote.latest_dcid(),
            ccf,
            paths: components.paths.clone(),
        }
    }

    pub fn should_send(&self) -> bool {
        let mut last_recv_time_guard = self.last_recv_time.lock().unwrap();
        self.rcvd_packets.fetch_add(1, Ordering::AcqRel);

        if self.rcvd_packets.load(Ordering::Acquire) >= 3
            || last_recv_time_guard.elapsed() > Duration::from_secs(1)
        {
            *last_recv_time_guard = tokio::time::Instant::now();
            self.rcvd_packets.store(0, Ordering::Release);
            true
        } else {
            false
        }
    }

    pub async fn try_send<W>(&self, mut write: W)
    where
        W: FnMut(
            &mut [u8],
            Option<ConnectionId>,
            Option<ConnectionId>,
            &ConnectionCloseFrame,
        ) -> Option<usize>,
    {
        for path in self.paths.iter() {
            let mut datagram = vec![0; path.mtu() as _];
            match write(&mut datagram, self.scid, self.dcid, &self.ccf) {
                Some(written) if written > 0 => {
                    _ = path
                        .send_packets(&[io::IoSlice::new(&datagram[..written])])
                        .await;
                }
                _ => {}
            };
        }
    }

    pub async fn try_send_with<W>(&self, pathway: Pathway, write: W)
    where
        W: FnOnce(
            &mut [u8],
            Option<ConnectionId>,
            Option<ConnectionId>,
            &ConnectionCloseFrame,
        ) -> Option<usize>,
    {
        let Some(path) = self.paths.get(&pathway) else {
            return;
        };

        let mut datagram = vec![0; path.mtu() as _];
        match write(&mut datagram, self.scid, self.dcid, &self.ccf) {
            Some(written) if written > 0 => {
                _ = path
                    .send_packets(&[io::IoSlice::new(&datagram[..written])])
                    .await;
            }
            _ => {}
        };
    }
}

#[derive(Clone)]
enum State {
    Closing(Arc<RcvdPacketQueue>),
    Draining,
}

#[derive(Clone)]
pub struct Termination {
    // for generate io::Error
    error: Error,
    // keep this to keep the routing
    _local_cids: ArcLocalCids,
    state: State,
}

impl Termination {
    pub fn closing(error: Error, local_cids: ArcLocalCids, state: Arc<RcvdPacketQueue>) -> Self {
        Self {
            error,
            _local_cids: local_cids,
            state: State::Closing(state),
        }
    }

    pub fn draining(error: Error, local_cids: ArcLocalCids) -> Self {
        Self {
            error,
            _local_cids: local_cids,
            state: State::Draining,
        }
    }

    pub fn error(&self) -> Error {
        self.error.clone()
    }

    // Close packets queues, dont send and receive any more packets.
    pub fn enter_draining(&mut self) {
        if let State::Closing(rcvd_pkt_q) = mem::replace(&mut self.state, State::Draining) {
            rcvd_pkt_q.close_all();
        }
    }
}
