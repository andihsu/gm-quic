use std::{sync::Arc, time::Duration};

use bytes::BufMut;
use qbase::{
    Epoch,
    cid::ConnectionId,
    error::{Error, QuicError},
    frame::{
        ConnectionCloseFrame, Frame, FrameReader, PathChallengeFrame, PathResponseFrame,
        ReceiveFrame, SendFrame,
    },
    net::{
        address::BindAddr,
        route::{Link, Pathway},
        tx::{ArcSendWakers, Signals},
    },
    packet::{
        self, FinalPacketLayout, MarshalFrame, PacketContains, PacketWriter,
        header::{
            GetType, OneRttHeader,
            long::{ZeroRttHeader, io::LongHeaderBuilder},
        },
        keys::{ArcKeys, ArcOneRttKeys, ArcOneRttPacketKeys, HeaderProtectionKeys},
        number::PacketNumber,
        signal::SpinBit,
        r#type::Type,
    },
    param::StoreParameter,
    sid::{ControlStreamsConcurrency, Role},
    util::BoundQueue,
};
use qcongestion::{Feedback, Transport};
use qevent::{
    quic::{
        PacketHeader, PacketType, QuicFramesCollector,
        recovery::{PacketLost, PacketLostTrigger},
        transport::PacketReceived,
    },
    telemetry::Instrument,
};
use qinterface::packet::{CipherPacket, PlainPacket};
use qrecovery::{crypto::CryptoStream, journal::ArcRcvdJournal};
#[cfg(feature = "unreliable")]
use qunreliable::DatagramFlow;
use tokio::sync::mpsc;
use tracing::Instrument as _;

use crate::{
    ArcReliableFrameDeque, Components, DataJournal, DataStreams, GuaranteedFrame,
    events::{ArcEventBroker, EmitEvent, Event},
    path::{Path, SendBuffer},
    space::{AckDataSpace, FlowControlledDataStreams, pipe},
    termination::Terminator,
    tx::{PacketBuffer, PaddablePacket, Transaction},
};

pub type CipherZeroRttPacket = CipherPacket<ZeroRttHeader>;
pub type PlainZeroRttPacket = PlainPacket<ZeroRttHeader>;
pub type ReceivedZeroRttFrom = (BindAddr, CipherZeroRttPacket, Pathway, Link);

pub type CipherOneRttPacket = CipherPacket<OneRttHeader>;
pub type PlainOneRttPacket = PlainPacket<OneRttHeader>;
pub type ReceivedOneRttFrom = (BindAddr, CipherOneRttPacket, Pathway, Link);

pub struct DataSpace {
    zero_rtt_keys: ArcKeys,
    one_rtt_keys: ArcOneRttKeys,
    crypto_stream: CryptoStream,
    streams: DataStreams,
    #[cfg(feature = "unreliable")]
    datagrams: DatagramFlow,
    journal: DataJournal,
    reliable_frames: ArcReliableFrameDeque,
}

impl DataSpace {
    pub fn new(
        role: Role,
        reliable_frames: ArcReliableFrameDeque,
        local_params: &impl StoreParameter,
        streams_ctrl: Box<dyn ControlStreamsConcurrency>,
        tx_wakers: ArcSendWakers,
        max_ack_delay: Duration,
    ) -> Self {
        Self {
            zero_rtt_keys: ArcKeys::new_pending(),
            one_rtt_keys: ArcOneRttKeys::new_pending(),
            journal: DataJournal::with_capacity(16, Some(max_ack_delay)),
            crypto_stream: CryptoStream::new(4096, 4096, tx_wakers.clone()),
            reliable_frames: reliable_frames.clone(),
            streams: DataStreams::new(
                role,
                local_params,
                streams_ctrl,
                reliable_frames,
                tx_wakers.clone(),
            ),
            #[cfg(feature = "unreliable")]
            datagrams: DatagramFlow::new(1024, tx_wakers),
        }
    }

    pub async fn decrypt_0rtt_packet(
        &self,
        packet: CipherZeroRttPacket,
    ) -> Option<Result<PlainZeroRttPacket, QuicError>> {
        match self.zero_rtt_keys.get_remote_keys().await {
            Some(keys) => packet.decrypt_long_packet(
                keys.remote.header.as_ref(),
                keys.remote.packet.as_ref(),
                |pn| self.journal.of_rcvd_packets().decode_pn(pn),
            ),
            None => {
                packet.drop_on_key_unavailable();
                None
            }
        }
    }

    pub async fn decrypt_1rtt_packet(
        &self,
        packet: CipherOneRttPacket,
    ) -> Option<Result<PlainOneRttPacket, QuicError>> {
        match self.one_rtt_keys.get_remote_keys().await {
            Some((hpk, pk)) => packet.decrypt_short_packet(hpk.as_ref(), &pk, |pn| {
                self.journal.of_rcvd_packets().decode_pn(pn)
            }),
            None => {
                packet.drop_on_key_unavailable();
                None
            }
        }
    }

    pub fn try_assemble_0rtt_packet(
        &self,
        tx: &mut Transaction<'_>,
        path_challenge_frames: &SendBuffer<PathChallengeFrame>,
        buf: &mut [u8],
    ) -> Result<(PaddablePacket, usize), Signals> {
        if self.one_rtt_keys.get_local_keys().is_some() {
            return Err(Signals::empty()); // not error, just skip 0rtt
        }

        let keys = self.zero_rtt_keys.get_local_keys().ok_or(Signals::KEYS)?;
        let (retran_timeout, expire_timeout) = tx.retransmit_and_expire_time(Epoch::Data);
        let sent_journal = self.journal.of_sent_packets();
        let mut packet = PacketBuffer::new_long(
            LongHeaderBuilder::with_cid(tx.dcid(), tx.scid()).zero_rtt(),
            buf,
            keys,
            &sent_journal,
        )?;

        let mut signals = Signals::empty();

        _ = path_challenge_frames
            .try_load_frames_into(&mut packet)
            .map_err(|s| signals |= s);
        _ = self
            .crypto_stream
            .outgoing()
            .try_load_data_into(&mut packet)
            .map_err(|s| signals |= s);
        // try to load reliable frames into this 0RTT packet to send
        _ = self
            .reliable_frames
            .try_load_frames_into(&mut packet)
            .map_err(|s| signals |= s);
        // try to load stream frames into this 0RTT packet to send
        let fresh_data = self
            .streams
            .try_load_data_into(&mut packet, tx.flow_limit())
            .map_err(|s| signals |= s)
            .unwrap_or_default();
        #[cfg(feature = "unreliable")]
        let _ = self
            .datagrams
            .try_load_data_into(&mut packet)
            .map_err(|s| signals |= s);

        // 错误是累积的，只有最后发现确实不能组成一个数据包时才真正返回错误
        Ok((
            packet
                .prepare_with_time(retran_timeout, expire_timeout)
                .map_err(|_| signals)?,
            fresh_data,
        ))
    }

    pub fn try_assemble_1rtt_packet(
        &self,
        tx: &mut Transaction<'_>,
        spin: SpinBit,
        path_challenge_frames: &SendBuffer<PathChallengeFrame>,
        path_response_frames: &SendBuffer<PathResponseFrame>,
        buf: &mut [u8],
    ) -> Result<(PaddablePacket, Option<u64>, usize), Signals> {
        let (hpk, pk) = self.one_rtt_keys.get_local_keys().ok_or(Signals::KEYS)?;
        let (key_phase, pk) = pk.lock_guard().get_local();
        let sent_journal = self.journal.of_sent_packets();
        // (1) may_loss被调用时cc已经被锁定，may_loss会尝试锁定sent_journal
        // (2) PacketMemory会持有sent_journal的guard，而need_ack会尝试锁定cc
        // 在PacketMemory存在时尝试锁定cc，可能会和 (1) 冲突:
        //   (1)持有cc，要锁定sent_journal；(2)持有sent_journal要锁定cc
        // 在多线程的情况下，可能会发生死锁。所以提前调用need_ack，避免交叉导致死锁
        let need_ack = tx.need_ack(Epoch::Data);
        let (retran_timeout, expire_timeout) = tx.retransmit_and_expire_time(Epoch::Data);
        let mut packet = PacketBuffer::new_short(
            OneRttHeader::new(spin, tx.dcid()),
            buf,
            hpk,
            pk,
            key_phase,
            &sent_journal,
        )?;

        let mut signals = Signals::empty();

        let ack = need_ack
            .or_else(|| {
                let rcvd_journal = self.journal.of_rcvd_packets();
                rcvd_journal.trigger_ack_frame()
            })
            .ok_or(Signals::TRANSPORT)
            .and_then(|(largest, rcvd_time)| {
                let rcvd_journal = self.journal.of_rcvd_packets();
                let ack_frame = rcvd_journal.gen_ack_frame_util(
                    packet.pn(),
                    largest,
                    rcvd_time,
                    packet.remaining_mut(),
                )?;
                packet.dump_ack_frame(ack_frame);
                Ok(largest)
            })
            .map_err(|s| signals |= s)
            .ok();

        _ = path_challenge_frames
            .try_load_frames_into(&mut packet)
            .map_err(|s| signals |= s);
        _ = path_response_frames
            .try_load_frames_into(&mut packet)
            .map_err(|s| signals |= s);
        _ = self
            .crypto_stream
            .outgoing()
            .try_load_data_into(&mut packet)
            .map_err(|s| signals |= s);
        // try to load reliable frames into this 1RTT packet to send
        _ = self
            .reliable_frames
            .try_load_frames_into(&mut packet)
            .map_err(|s| signals |= s);
        // try to load stream frames into this 1RTT packet to send
        let fresh_data = self
            .streams
            .try_load_data_into(&mut packet, tx.flow_limit())
            .map_err(|s| signals |= s)
            .unwrap_or_default();

        #[cfg(feature = "unreliable")]
        let _ = self
            .datagrams
            .try_load_data_into(&mut packet)
            .map_err(|s| signals |= s);

        Ok((
            packet
                .prepare_with_time(retran_timeout, expire_timeout)
                .map_err(|_| signals)?,
            ack,
            fresh_data,
        ))
    }

    pub fn try_assemble_probe_packet(
        &self,
        tx: &mut Transaction<'_>,
        spin: SpinBit,
        path_challenge_frames: &SendBuffer<PathChallengeFrame>,
        path_response_frames: &SendBuffer<PathResponseFrame>,
        buf: &mut [u8],
    ) -> Result<PaddablePacket, Signals> {
        let (hpk, pk) = self.one_rtt_keys.get_local_keys().ok_or(Signals::KEYS)?;
        let (key_phase, pk) = pk.lock_guard().get_local();
        let (retran_timeout, expire_timeout) = tx.retransmit_and_expire_time(Epoch::Data);
        let sent_journal = self.journal.of_sent_packets();
        let mut packet = PacketBuffer::new_short(
            OneRttHeader::new(spin, tx.dcid()),
            buf,
            hpk,
            pk,
            key_phase,
            &sent_journal,
        )?;

        let mut signals = Signals::empty();
        _ = path_challenge_frames
            .try_load_frames_into(&mut packet)
            .map_err(|s| signals |= s);
        _ = path_response_frames
            .try_load_frames_into(&mut packet)
            .map_err(|s| signals |= s);
        // 其实还应该加上NCIDF，但是从ReliableFrameDeque分拣太复杂了

        packet
            .prepare_with_time(retran_timeout, expire_timeout)
            .map_err(|_| signals)
    }

    pub fn try_assemble_ping_packet(
        &self,
        tx: &mut Transaction<'_>,
        spin: SpinBit,
        buf: &mut [u8],
    ) -> Result<PaddablePacket, Signals> {
        let (hpk, pk) = self.one_rtt_keys.get_local_keys().ok_or(Signals::KEYS)?;
        let (key_phase, pk) = pk.lock_guard().get_local();
        let (retran_timeout, expire_timeout) = tx.retransmit_and_expire_time(Epoch::Data);
        let sent_journal = self.journal.of_sent_packets();
        let mut packet = PacketBuffer::new_short(
            OneRttHeader::new(spin, tx.dcid()),
            buf,
            hpk,
            pk,
            key_phase,
            &sent_journal,
        )?;

        packet.dump_ping_frame();

        packet
            .prepare_with_time(retran_timeout, expire_timeout)
            .map_err(|_| unreachable!("packet is not empty"))
    }

    pub fn is_one_rtt_ready(&self) -> bool {
        self.one_rtt_keys.get_local_keys().is_some()
    }

    pub fn one_rtt_keys(&self) -> ArcOneRttKeys {
        self.one_rtt_keys.clone()
    }

    pub fn on_conn_error(&self, error: &Error) {
        self.streams.on_conn_error(error);
        #[cfg(feature = "unreliable")]
        self.datagrams.on_conn_error(error);
    }

    pub fn crypto_stream(&self) -> &CryptoStream {
        &self.crypto_stream
    }

    pub fn streams(&self) -> &DataStreams {
        &self.streams
    }

    #[cfg(feature = "unreliable")]
    pub fn datagrams(&self) -> &DatagramFlow {
        &self.datagrams
    }
}

pub fn spawn_deliver_and_parse(
    zeor_rtt_packets: BoundQueue<ReceivedZeroRttFrom>,
    one_rtt_packets: BoundQueue<ReceivedOneRttFrom>,
    space: Arc<DataSpace>,
    components: &Components,
    event_broker: ArcEventBroker,
) {
    let (ack_frames_entry, rcvd_ack_frames) = mpsc::unbounded_channel();
    // 连接级的
    let (max_data_frames_entry, rcvd_max_data_frames) = mpsc::unbounded_channel();
    let (data_blocked_frames_entry, rcvd_data_blocked_frames) = mpsc::unbounded_channel();
    let (new_cid_frames_entry, rcvd_new_cid_frames) = mpsc::unbounded_channel();
    let (retire_cid_frames_entry, rcvd_retire_cid_frames) = mpsc::unbounded_channel();
    let (handshake_done_frames_entry, rcvd_handshake_done_frames) = mpsc::unbounded_channel();
    let (new_token_frames_entry, rcvd_new_token_frames) = mpsc::unbounded_channel();
    // 数据级的
    let (crypto_frames_entry, rcvd_crypto_frames) = mpsc::unbounded_channel();
    let (stream_ctrl_frames_entry, rcvd_stream_ctrl_frames) = mpsc::unbounded_channel();
    let (stream_frames_entry, rcvd_stream_frames) = mpsc::unbounded_channel();
    #[cfg(feature = "unreliable")]
    let (datagram_frames_entry, rcvd_datagram_frames) = mpsc::unbounded_channel();

    let flow_controlled_data_streams =
        FlowControlledDataStreams::new(space.streams.clone(), components.flow_ctrl.clone());

    // Assemble the pipelines of frame processing
    pipe(
        rcvd_retire_cid_frames,
        components.cid_registry.local.clone(),
        event_broker.clone(),
    );
    pipe(
        rcvd_new_cid_frames,
        components.cid_registry.remote.clone(),
        event_broker.clone(),
    );
    pipe(
        rcvd_max_data_frames,
        components.flow_ctrl.sender.clone(),
        event_broker.clone(),
    );
    pipe(
        rcvd_data_blocked_frames,
        components.flow_ctrl.recver.clone(),
        event_broker.clone(),
    );
    pipe(
        rcvd_handshake_done_frames,
        components
            .handshake
            .discard_spaces_on_client_handshake_done(components.paths.clone()),
        event_broker.clone(),
    );
    pipe(
        rcvd_crypto_frames,
        space.crypto_stream.incoming(),
        event_broker.clone(),
    );
    pipe(
        rcvd_stream_ctrl_frames,
        flow_controlled_data_streams.clone(),
        event_broker.clone(),
    );
    pipe(
        rcvd_stream_frames,
        flow_controlled_data_streams,
        event_broker.clone(),
    );
    #[cfg(feature = "unreliable")]
    pipe(
        rcvd_datagram_frames,
        space.datagrams.clone(),
        event_broker.clone(),
    );
    pipe(
        rcvd_ack_frames,
        AckDataSpace::new(&space.journal, &space.streams, &space.crypto_stream),
        event_broker.clone(),
    );
    pipe(
        rcvd_new_token_frames,
        components.token_registry.clone(),
        event_broker.clone(),
    );

    let dispatch_data_frame = {
        let event_broker = event_broker.clone();
        let rcvd_joural = space.journal.of_rcvd_packets();
        move |frame: Frame, pty: packet::Type, path: &Path| match frame {
            Frame::Ack(f) => {
                path.cc().on_ack_rcvd(Epoch::Data, &f);
                rcvd_joural.on_rcvd_ack(&f);
                _ = ack_frames_entry.send(f)
            }
            Frame::NewToken(f) => _ = new_token_frames_entry.send(f),
            Frame::MaxData(f) => _ = max_data_frames_entry.send(f),
            Frame::NewConnectionId(f) => _ = new_cid_frames_entry.send(f),
            Frame::RetireConnectionId(f) => _ = retire_cid_frames_entry.send(f),
            Frame::HandshakeDone(f) => {
                _ = {
                    // See [Section 4.1.2](https://datatracker.ietf.org/doc/html/rfc9001#handshake-confirmed)
                    handshake_done_frames_entry.send(f)
                }
            }
            Frame::DataBlocked(f) => _ = data_blocked_frames_entry.send(f),
            Frame::Challenge(f) => _ = path.recv_frame(&f),
            Frame::Response(f) => _ = path.recv_frame(&f),
            Frame::StreamCtl(f) => _ = stream_ctrl_frames_entry.send(f),
            Frame::Stream(f, data) => _ = stream_frames_entry.send((f, data)),
            Frame::Crypto(f, bytes) => _ = crypto_frames_entry.send((f, bytes)),
            #[cfg(feature = "unreliable")]
            Frame::Datagram(f, data) => _ = datagram_frames_entry.send((f, data)),
            Frame::Close(f) if matches!(pty, Type::Short(_)) => event_broker.emit(Event::Closed(f)),
            _ => {}
        }
    };

    let deliver_and_parse_0rtt = {
        let components = components.clone();
        let space = space.clone();
        let dispatch_data_frame = dispatch_data_frame.clone();
        let event_broker = event_broker.clone();
        async move {
            while let Some((bind_addr, packet, pathway, link)) = zeor_rtt_packets.recv().await {
                let parse = async {
                    let _qlog_span = qevent::span!(@current, path=pathway.to_string()).enter();
                    if let Some(packet) = space.decrypt_0rtt_packet(packet).await.transpose()? {
                        let path = match components
                            .get_or_try_create_path(bind_addr, link, pathway, true)
                        {
                            Ok(path) => path,
                            Err(_) => {
                                packet.drop_on_conenction_closed();
                                return Ok(());
                            }
                        };

                        let mut frames = QuicFramesCollector::<PacketReceived>::new();
                        let packet_contains = FrameReader::new(packet.body(), packet.get_type())
                            .try_fold(PacketContains::default(), |packet_contains, frame| {
                                let (frame, frame_type) = frame?;
                                frames.extend(Some(&frame));
                                dispatch_data_frame(frame, packet.get_type(), &path);
                                Result::<_, QuicError>::Ok(packet_contains.include(frame_type))
                            })?;
                        packet.log_received(frames);

                        space.journal.of_rcvd_packets().register_pn(
                            packet.pn(),
                            packet_contains.ack_eliciting(),
                            path.cc().get_pto(Epoch::Data),
                        );
                        path.on_packet_rcvd(
                            Epoch::Data,
                            packet.pn(),
                            packet.size(),
                            packet_contains,
                        );
                    };

                    Result::<(), Error>::Ok(())
                };
                if let Err(Error::Quic(error)) = parse.await {
                    event_broker.emit(Event::Failed(error));
                };
            }
        }
    };

    let deliver_and_parse_1rtt = {
        let components = components.clone();
        let space = space.clone();
        let dispatch_data_frame = dispatch_data_frame.clone();
        let event_broker = event_broker.clone();
        async move {
            while let Some((bind_addr, packet, pathway, link)) = one_rtt_packets.recv().await {
                let parse = async {
                    let _qlog_span = qevent::span!(@current, path=pathway.to_string()).enter();
                    if let Some(packet) = space.decrypt_1rtt_packet(packet).await.transpose()? {
                        let path = match components
                            .get_or_try_create_path(bind_addr, link, pathway, true)
                        {
                            Ok(path) => path,
                            Err(_) => {
                                packet.drop_on_conenction_closed();
                                return Ok(());
                            }
                        };
                        components
                            .handshake
                            .discard_spaces_on_server_handshake_done(&components.paths);

                        let mut frames = QuicFramesCollector::<PacketReceived>::new();
                        let packet_contains = FrameReader::new(packet.body(), packet.get_type())
                            .try_fold(PacketContains::default(), |packet_contains, frame| {
                                let (frame, frame_type) = frame?;
                                frames.extend(Some(&frame));
                                dispatch_data_frame(frame, packet.get_type(), &path);
                                Result::<_, QuicError>::Ok(packet_contains.include(frame_type))
                            })?;
                        packet.log_received(frames);

                        space.journal.of_rcvd_packets().register_pn(
                            packet.pn(),
                            packet_contains.ack_eliciting(),
                            path.cc().get_pto(Epoch::Data),
                        );
                        path.on_packet_rcvd(
                            Epoch::Data,
                            packet.pn(),
                            packet.size(),
                            packet_contains,
                        );
                    }
                    Result::<(), Error>::Ok(())
                };
                if let Err(Error::Quic(error)) = parse.await {
                    event_broker.emit(Event::Failed(error));
                };
            }
        }
    };

    tokio::spawn({
        let conn_state = components.conn_state.clone();
        async move {
            tokio::select! {
                _ = deliver_and_parse_0rtt => {},
                _ = conn_state.terminated() => {}
            };
        }
        .instrument_in_current()
        .in_current_span()
    });
    tokio::spawn({
        let conn_state = components.conn_state.clone();
        async move {
            tokio::select! {
                _ = deliver_and_parse_1rtt => {},
                _ = conn_state.terminated() => {}
            };
        }
        .instrument_in_current()
        .in_current_span()
    });
}

impl Feedback for DataSpace {
    fn may_loss(&self, trigger: PacketLostTrigger, pns: &mut dyn Iterator<Item = u64>) {
        let sent_jornal = self.journal.of_sent_packets();
        let crypto_outgoing = self.crypto_stream.outgoing();
        let mut sent_packets = sent_jornal.rotate();
        for pn in pns {
            let mut may_lost_frames = QuicFramesCollector::<PacketLost>::new();
            for frame in sent_packets.may_loss_packet(pn) {
                match frame {
                    GuaranteedFrame::Crypto(frame) => {
                        may_lost_frames.extend([&frame]);
                        crypto_outgoing.may_loss_data(&frame);
                    }
                    GuaranteedFrame::Stream(frame) => {
                        may_lost_frames.extend([&frame]);
                        self.streams.may_loss_data(&frame);
                    }
                    GuaranteedFrame::Reliable(frame) => {
                        may_lost_frames.extend([&frame]);
                        self.reliable_frames.send_frame([frame]);
                    }
                };
            }
            qevent::event!(PacketLost {
                header: PacketHeader {
                    // TOOD: 如果只有支持0rtt，这里就不一定是1rtt了
                    packet_type: PacketType::OneRTT,
                    packet_number: pn
                },
                frames: may_lost_frames,
                trigger
            });
        }
    }
}

#[derive(Clone)]
pub struct ClosingDataSpace {
    keys: (HeaderProtectionKeys, ArcOneRttPacketKeys),
    ccf_packet_pn: (u64, PacketNumber),
    rcvd_journal: ArcRcvdJournal,
}

impl DataSpace {
    pub fn close(&self) -> Option<ClosingDataSpace> {
        let keys = self.one_rtt_keys.invalid()?;
        let sent_journal = self.journal.of_sent_packets();
        let new_packet_guard = sent_journal.new_packet();
        let ccf_packet_pn = new_packet_guard.pn();
        let rcvd_journal = self.journal.of_rcvd_packets();
        Some(ClosingDataSpace {
            rcvd_journal,
            ccf_packet_pn,
            keys,
        })
    }
}

impl ClosingDataSpace {
    pub fn recv_packet(&self, packet: CipherOneRttPacket) -> Option<ConnectionCloseFrame> {
        let packet = packet
            .decrypt_short_packet(self.keys.0.remote.as_ref(), &self.keys.1, |pn| {
                self.rcvd_journal.decode_pn(pn)
            })
            .and_then(Result::ok)?;

        let mut frames = QuicFramesCollector::<PacketReceived>::new();
        let ccf = FrameReader::new(packet.body(), packet.get_type())
            .filter_map(Result::ok)
            .inspect(|(f, _ack)| frames.extend(Some(f)))
            .fold(None, |ccf, (frame, _)| match (ccf, frame) {
                (ccf @ Some(..), _) => ccf,
                (None, Frame::Close(ccf)) => Some(ccf),
                (None, _) => None,
            });
        packet.log_received(frames);
        ccf
    }

    pub fn try_assemble_ccf_packet(
        &self,
        dcid: ConnectionId,
        ccf: &ConnectionCloseFrame,
        buf: &mut [u8],
    ) -> Option<FinalPacketLayout> {
        let (hpk, pk) = &self.keys;
        let (key_phase, pk) = pk.lock_guard().get_local();
        let header = OneRttHeader::new(Default::default(), dcid);
        let pn = self.ccf_packet_pn;
        // 装填ccf时ccf不在乎Limiter
        let mut packet_writer =
            PacketWriter::new_short(&header, buf, pn, hpk.local.clone(), pk, key_phase).ok()?;

        packet_writer.dump_frame(ccf.clone());

        Some(packet_writer.encrypt_and_protect())
    }
}

pub fn spawn_deliver_and_parse_closing(
    packets: BoundQueue<ReceivedOneRttFrom>,
    space: ClosingDataSpace,
    terminator: Arc<Terminator>,
    event_broker: ArcEventBroker,
) {
    tokio::spawn(
        async move {
            while let Some((_, packet, pathway, _socket)) = packets.recv().await {
                if let Some(ccf) = space.recv_packet(packet) {
                    event_broker.emit(Event::Closed(ccf.clone()));
                    return;
                }
                if terminator.should_send() {
                    _ = terminator
                        .try_send_with(pathway, |buf, _scid, dcid, ccf| {
                            space
                                .try_assemble_ccf_packet(dcid?, ccf, buf)
                                .map(|layout| layout.sent_bytes())
                        })
                        .await;
                }
            }
        }
        .instrument_in_current()
        .in_current_span(),
    );
}
