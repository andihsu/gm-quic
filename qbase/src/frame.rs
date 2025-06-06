use std::fmt::Debug;

use bytes::{Buf, BufMut, Bytes};
use derive_more::{Deref, DerefMut};
use enum_dispatch::enum_dispatch;
use io::WriteFrame;

use super::varint::VarInt;
use crate::packet::r#type::Type;

mod ack;
mod connection_close;
mod crypto;
mod data_blocked;
mod datagram;
mod handshake_done;
mod max_data;
mod max_stream_data;
mod max_streams;
mod new_connection_id;
mod new_token;
mod padding;
pub mod path_challenge;
mod path_response;
mod ping;
mod reset_stream;
mod retire_connection_id;
mod stop_sending;
mod stream;
mod stream_data_blocked;
mod streams_blocked;

/// Error module for parsing frames
pub mod error;
/// IO module for frame encoding and decoding
pub mod io;

pub use ack::{AckFrame, EcnCounts};
pub use connection_close::{AppCloseFrame, ConnectionCloseFrame, QuicCloseFrame};
pub use crypto::CryptoFrame;
pub use data_blocked::DataBlockedFrame;
pub use datagram::DatagramFrame;
#[doc(hidden)]
pub use error::Error;
pub use handshake_done::HandshakeDoneFrame;
pub use max_data::MaxDataFrame;
pub use max_stream_data::MaxStreamDataFrame;
pub use max_streams::MaxStreamsFrame;
pub use new_connection_id::NewConnectionIdFrame;
pub use new_token::NewTokenFrame;
pub use padding::PaddingFrame;
pub use path_challenge::PathChallengeFrame;
pub use path_response::PathResponseFrame;
pub use ping::PingFrame;
pub use reset_stream::{ResetStreamError, ResetStreamFrame};
pub use retire_connection_id::RetireConnectionIdFrame;
pub use stop_sending::StopSendingFrame;
pub use stream::{EncodingStrategy, STREAM_FRAME_MAX_ENCODING_SIZE, StreamFrame};
pub use stream_data_blocked::StreamDataBlockedFrame;
pub use streams_blocked::StreamsBlockedFrame;

/// Define the basic behaviors for all kinds of frames
#[enum_dispatch]
pub trait GetFrameType: Debug {
    /// Return the type of frame
    fn frame_type(&self) -> FrameType;
}

#[enum_dispatch]
pub trait EncodeFrame {
    /// Return the max number of bytes needed to encode this value
    ///
    /// Calculate the maximum size by summing up the maximum length of each field.
    /// If a field type has a maximum length, use it, otherwise use the actual length
    /// of the data in that field.
    ///
    /// When packaging data, by pre-estimating this value to effectively avoid spending
    /// extra resources to calculate the actual encoded size.
    fn max_encoding_size(&self) -> usize {
        1
    }

    /// Return the exact number of bytes needed to encode this value
    fn encoding_size(&self) -> usize {
        1
    }
}

/// The `Spec` summarizes any special rules governing the processing
/// or generation of the frame type, as indicated by the following characters.
///
/// See [table-3](https://www.rfc-editor.org/rfc/rfc9000.html#table-3)
/// of [QUIC](https://www.rfc-editor.org/rfc/rfc9000.html) for more details.
pub enum Spec {
    /// Packets containing only frames with this marking are not ack-eliciting.
    ///
    /// See [Section 13.2](https://www.rfc-editor.org/rfc/rfc9000.html#generating-acks)
    /// of [QUIC](https://www.rfc-editor.org/rfc/rfc9000.html) for more details.
    NonAckEliciting = 1,
    /// Packets containing only frames with this marking do not count toward bytes
    /// in flight for congestion control purposes.
    /// See [section-12.4-14.4](https://www.rfc-editor.org/rfc/rfc9000.html#section-12.4-14.4)
    /// of [QUIC](https://www.rfc-editor.org/rfc/rfc9000.html).
    ///
    /// Similar to TCP, packets containing only ACK frames do not count toward bytes
    /// in flight and are not congestion controlled.
    /// See [Section 7.4](https://www.rfc-editor.org/rfc/rfc9002#section-7-4)
    /// of [QUIC-RECOVERY](https://www.rfc-editor.org/rfc/rfc9002).
    CongestionControlFree = 2,
    /// Packets containing only frames with this marking can be used to probe
    /// new network paths during connection migration.
    ///
    /// See [Section 9.1](https://www.rfc-editor.org/rfc/rfc9000.html#probing)
    /// of [QUIC](https://www.rfc-editor.org/rfc/rfc9000.html).
    ProbeNewPath = 4,
    /// The contents of frames with this marking are flow controlled.
    ///
    /// See [Section 4](https://www.rfc-editor.org/rfc/rfc9000.html#flow-control)
    /// of [QUIC](https://www.rfc-editor.org/rfc/rfc9000.html) for more details.
    FlowControlled = 8,
}

pub trait ContainSpec {
    fn contain(&self, spec: Spec) -> bool;
}

impl ContainSpec for u8 {
    #[inline]
    fn contain(&self, spec: Spec) -> bool {
        *self & spec as u8 != 0
    }
}

/// The sum type of all the core QUIC frame types.
///
/// See [table-3](https://www.rfc-editor.org/rfc/rfc9000.html#table-3)
/// and [frame types and formats](https://www.rfc-editor.org/rfc/rfc9000.html#name-frame-types-and-formats)
/// of [QUIC](https://www.rfc-editor.org/rfc/rfc9000.html) for more details.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum FrameType {
    /// PADDED frame, see [`PaddingFrame`].
    Padding,
    /// PING frame, see [`PingFrame`].
    Ping,
    /// ACK frame, see [`AckFrame`].
    Ack(u8),
    /// RESET_STREAM frame, see [`ResetStreamFrame`].
    ResetStream,
    /// STOP_SENDING frame, see [`StopSendingFrame`].
    StopSending,
    /// CRYPTO frame, see [`CryptoFrame`].
    Crypto,
    /// NEW_TOKEN frame, see [`NewTokenFrame`].
    NewToken,
    /// STREAM frame, see [`StreamFrame`].
    Stream(u8),
    /// MAX_DATA frame, see [`MaxDataFrame`].
    MaxData,
    /// MAX_STREAM_DATA frame, see [`MaxStreamDataFrame`].
    MaxStreamData,
    /// MAX_STREAMS frame, see [`MaxStreamsFrame`].
    MaxStreams(u8),
    /// DATA_BLOCKED frame, see [`DataBlockedFrame`].
    DataBlocked,
    /// STREAM_DATA_BLOCKED frame, see [`StreamDataBlockedFrame`].
    StreamDataBlocked,
    /// STREAMS_BLOCKED frame, see [`StreamsBlockedFrame`].
    StreamsBlocked(u8),
    /// NEW_CONNECTION_ID frame, see [`NewConnectionIdFrame`].
    NewConnectionId,
    /// RETIRE_CONNECTION_ID frame, see [`RetireConnectionIdFrame`].
    RetireConnectionId,
    /// PATH_CHALLENGE frame, see [`PathChallengeFrame`].
    PathChallenge,
    /// PATH_RESPONSE frame, see [`PathResponseFrame`].
    PathResponse,
    /// CONNECTION_CLOSE frame, see [`ConnectionCloseFrame`].
    ConnectionClose(u8),
    /// HANDSHAKE_DONE frame, see [`HandshakeDoneFrame`].
    HandshakeDone,
    /// DATAGRAM frame, see [`DatagramFrame`].
    Datagram(u8),
}

#[enum_dispatch]
pub trait FrameFeture {
    /// Return whether a frame type belongs to the given packet_type
    fn belongs_to(&self, packet_type: Type) -> bool;
    /// Return the specs of the frame type
    fn specs(&self) -> u8;
}

impl<T: GetFrameType> FrameFeture for T {
    fn belongs_to(&self, packet_type: Type) -> bool {
        self.frame_type().belongs_to(packet_type)
    }

    fn specs(&self) -> u8 {
        self.frame_type().specs()
    }
}

impl FrameFeture for FrameType {
    fn belongs_to(&self, packet_type: Type) -> bool {
        use crate::packet::r#type::{
            long::{Type::V1, Ver1},
            short::OneRtt,
        };
        // IH01
        let i = matches!(packet_type, Type::Long(V1(Ver1::INITIAL)));
        let h = matches!(packet_type, Type::Long(V1(Ver1::HANDSHAKE)));
        let o = matches!(packet_type, Type::Long(V1(Ver1::ZERO_RTT)));
        let l = matches!(packet_type, Type::Short(OneRtt(_)));

        match self {
            FrameType::Padding => i | h | o | l,
            FrameType::Ping => i | h | o | l,
            FrameType::Ack(_) => i | h | l,
            FrameType::ResetStream => o | l,
            FrameType::StopSending => o | l,
            FrameType::Crypto => i | h | l,
            FrameType::NewToken => l,
            FrameType::Stream(_) => o | l,
            FrameType::MaxData => o | l,
            FrameType::MaxStreamData => o | l,
            FrameType::MaxStreams(_) => o | l,
            FrameType::DataBlocked => o | l,
            FrameType::StreamDataBlocked => o | l,
            FrameType::StreamsBlocked(_) => o | l,
            FrameType::NewConnectionId => o | l,
            FrameType::RetireConnectionId => o | l,
            FrameType::PathChallenge => o | l,
            FrameType::PathResponse => l,
            // The application-specific variant of CONNECTION_CLOSE (type 0x1d) can only be
            // sent using 0-RTT or 1-RTT packets;
            // See [Section 12.5](https://www.rfc-editor.org/rfc/rfc9000.html#section-12.5).
            //
            // When an application wishes to abandon a connection during the handshake,
            // an endpoint can send a CONNECTION_CLOSE frame (type 0x1c) with an error code
            // of APPLICATION_ERROR in an Initial or Handshake packet.
            FrameType::ConnectionClose(bit) => {
                if *bit == 0 && i || h {
                    true
                } else {
                    o | l
                }
            }
            FrameType::HandshakeDone => l,
            FrameType::Datagram(_) => o | l,
        }
    }

    fn specs(&self) -> u8 {
        let (n, c, p, f) = (
            Spec::NonAckEliciting as u8,
            Spec::CongestionControlFree as u8,
            Spec::ProbeNewPath as u8,
            Spec::FlowControlled as u8,
        );
        match self {
            FrameType::Padding => n | p,
            FrameType::Ack(_) => n | c,
            FrameType::Stream(_) => f,
            FrameType::NewConnectionId => p,
            FrameType::PathChallenge => p,
            FrameType::PathResponse => p,
            // different from [table 3](https://www.rfc-editor.org/rfc/rfc9000.html#table-3),
            // add the [`Spec::Con`] for the CONNECTION_CLOSE frame
            FrameType::ConnectionClose(_) => n | c,
            _ => 0,
        }
    }
}

impl TryFrom<VarInt> for FrameType {
    type Error = Error;

    fn try_from(frame_type: VarInt) -> Result<Self, Self::Error> {
        Ok(match frame_type.into_inner() {
            0x00 => FrameType::Padding,
            0x01 => FrameType::Ping,
            // The last bit is the ECN flag.
            ty @ (0x02 | 0x03) => FrameType::Ack(ty as u8 & 0b1),
            0x04 => FrameType::ResetStream,
            0x05 => FrameType::StopSending,
            0x06 => FrameType::Crypto,
            0x07 => FrameType::NewToken,
            // The last three bits are the offset, length, and fin flag bits respectively.
            ty @ 0x08..=0x0f => FrameType::Stream(ty as u8 & 0b111),
            0x10 => FrameType::MaxData,
            0x11 => FrameType::MaxStreamData,
            // The last bit is the direction flag bit, 0 indicates bidirectional, 1 indicates unidirectional.
            ty @ (0x12 | 0x13) => FrameType::MaxStreams(ty as u8 & 0b1),
            0x14 => FrameType::DataBlocked,
            0x15 => FrameType::StreamDataBlocked,
            // The last bit is the direction flag bit, 0 indicates bidirectional, 1 indicates unidirectional.
            ty @ (0x16 | 0x17) => FrameType::StreamsBlocked(ty as u8 & 0b1),
            0x18 => FrameType::NewConnectionId,
            0x19 => FrameType::RetireConnectionId,
            0x1a => FrameType::PathChallenge,
            0x1b => FrameType::PathResponse,
            // The last bit is the layer flag bit, 0 indicates application layer, 1 indicates transport layer.
            ty @ (0x1c | 0x1d) => FrameType::ConnectionClose(ty as u8 & 0x1),
            0x1e => FrameType::HandshakeDone,
            // The last bit is the length flag bit, 0 the length field is absent and the Datagram Data
            // field extends to the end of the packet, 1 the length field is present.
            ty @ (0x30 | 0x31) => FrameType::Datagram(ty as u8 & 1),
            // May be extension frame
            _ => return Err(Self::Error::InvalidType(frame_type)),
        })
    }
}

impl From<FrameType> for VarInt {
    fn from(frame_type: FrameType) -> Self {
        match frame_type {
            FrameType::Padding => VarInt::from_u32(0x00),
            FrameType::Ping => VarInt::from_u32(0x01),
            FrameType::Ack(ecn) => VarInt::from(0x02 | ecn),
            FrameType::ResetStream => VarInt::from_u32(0x04),
            FrameType::StopSending => VarInt::from_u32(0x05),
            FrameType::Crypto => VarInt::from_u32(0x06),
            FrameType::NewToken => VarInt::from_u32(0x07),
            FrameType::Stream(flag) => VarInt::from(0x08 | flag),
            FrameType::MaxData => VarInt::from_u32(0x10),
            FrameType::MaxStreamData => VarInt::from_u32(0x11),
            FrameType::MaxStreams(dir) => VarInt::from(0x12 | dir),
            FrameType::DataBlocked => VarInt::from_u32(0x14),
            FrameType::StreamDataBlocked => VarInt::from_u32(0x15),
            FrameType::StreamsBlocked(dir) => VarInt::from(0x16 | dir),
            FrameType::NewConnectionId => VarInt::from_u32(0x18),
            FrameType::RetireConnectionId => VarInt::from_u32(0x19),
            FrameType::PathChallenge => VarInt::from_u32(0x1a),
            FrameType::PathResponse => VarInt::from_u32(0x1b),
            FrameType::ConnectionClose(layer) => VarInt::from(0x1c | layer),
            FrameType::HandshakeDone => VarInt::from_u32(0x1e),
            FrameType::Datagram(with_len) => VarInt::from(0x30 | with_len),
        }
    }
}

/// Parse the frame type from the input buffer,
/// [nom](https://docs.rs/nom/latest/nom/) parser style.
pub fn be_frame_type(input: &[u8]) -> nom::IResult<&[u8], FrameType, Error> {
    let (remain, frame_type) = crate::varint::be_varint(input).map_err(|_| {
        nom::Err::Error(Error::IncompleteType(format!(
            "Incomplete frame type from input: {input:?}"
        )))
    })?;
    let frame_type = FrameType::try_from(frame_type).map_err(nom::Err::Error)?;
    Ok((remain, frame_type))
}

/// Sum type of all the stream related frames except [`StreamFrame`].
#[derive(Debug, Clone, Eq, PartialEq)]
#[enum_dispatch(EncodeFrame, GetFrameType)]
pub enum StreamCtlFrame {
    /// RESET_STREAM frame, see [`ResetStreamFrame`].
    ResetStream(ResetStreamFrame),
    /// STOP_SENDING frame, see [`StopSendingFrame`].
    StopSending(StopSendingFrame),
    /// MAX_STREAM_DATA frame, see [`MaxStreamDataFrame`].
    MaxStreamData(MaxStreamDataFrame),
    /// MAX_STREAMS frame, see [`MaxStreamsFrame`].
    MaxStreams(MaxStreamsFrame),
    /// STREAM_DATA_BLOCKED frame, see [`StreamDataBlockedFrame`].
    StreamDataBlocked(StreamDataBlockedFrame),
    /// STREAMS_BLOCKED frame, see [`StreamsBlockedFrame`].
    StreamsBlocked(StreamsBlockedFrame),
}

/// Sum type of all the reliable frames.
#[derive(Debug, Clone, Eq, PartialEq)]
#[enum_dispatch(EncodeFrame, GetFrameType)]
pub enum ReliableFrame {
    /// NEW_TOKEN frame, see [`NewTokenFrame`].
    NewToken(NewTokenFrame),
    /// MAX_DATA frame, see [`MaxDataFrame`].
    MaxData(MaxDataFrame),
    /// DATA_BLOCKED frame, see [`DataBlockedFrame`].
    DataBlocked(DataBlockedFrame),
    /// NEW_CONNECTION_ID frame, see [`NewConnectionIdFrame`].
    NewConnectionId(NewConnectionIdFrame),
    /// RETIRE_CONNECTION_ID frame, see [`RetireConnectionIdFrame`].
    RetireConnectionId(RetireConnectionIdFrame),
    /// HANDSHAKE_DONE frame, see [`HandshakeDoneFrame`].
    HandshakeDone(HandshakeDoneFrame),
    /// STREAM control frame, see [`StreamCtlFrame`].
    Stream(StreamCtlFrame),
}

/// Sum type of all the frames.
///
/// The data frames' body are stored in the second field.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Frame {
    /// PADDING frame, see [`PaddingFrame`].
    Padding(PaddingFrame),
    /// PING frame, see [`PingFrame`].
    Ping(PingFrame),
    /// ACK frame, see [`AckFrame`].
    Ack(AckFrame),
    /// CONNECTION_CLOSE frame, see [`ConnectionCloseFrame`].
    Close(ConnectionCloseFrame),
    /// NEW_TOKEN frame, see [`NewTokenFrame`].
    NewToken(NewTokenFrame),
    /// MAX_DATA frame, see [`MaxDataFrame`].
    MaxData(MaxDataFrame),
    /// DATA_BLOCKED frame, see [`DataBlockedFrame`].
    DataBlocked(DataBlockedFrame),
    /// NEW_CONNECTION_ID frame, see [`NewConnectionIdFrame`].
    NewConnectionId(NewConnectionIdFrame),
    /// RETIRE_CONNECTION_ID frame, see [`RetireConnectionIdFrame`].
    RetireConnectionId(RetireConnectionIdFrame),
    /// HANDSHAKE_DONE frame, see [`HandshakeDoneFrame`].
    HandshakeDone(HandshakeDoneFrame),
    /// PATH_CHALLENGE frame, see [`PathChallengeFrame`].
    Challenge(PathChallengeFrame),
    /// PATH_RESPONSE frame, see [`PathResponseFrame`].
    Response(PathResponseFrame),
    /// Stream control frame, see [`StreamCtlFrame`].
    StreamCtl(StreamCtlFrame),
    /// STREAM frame and its data, see [`StreamFrame`].
    Stream(StreamFrame, Bytes),
    /// CRYPTO frame and its data, see [`CryptoFrame`].
    Crypto(CryptoFrame, Bytes),
    /// DATAGRAM frame and its data, see [`DatagramFrame`].
    Datagram(DatagramFrame, Bytes),
}

impl From<ReliableFrame> for Frame {
    fn from(frame: ReliableFrame) -> Self {
        match frame {
            ReliableFrame::NewToken(new_token_frame) => Frame::NewToken(new_token_frame),
            ReliableFrame::MaxData(max_data_frame) => Frame::MaxData(max_data_frame),
            ReliableFrame::DataBlocked(data_blocked_frame) => {
                Frame::DataBlocked(data_blocked_frame)
            }
            ReliableFrame::NewConnectionId(new_connection_id_frame) => {
                Frame::NewConnectionId(new_connection_id_frame)
            }
            ReliableFrame::RetireConnectionId(retire_connection_id_frame) => {
                Frame::RetireConnectionId(retire_connection_id_frame)
            }
            ReliableFrame::HandshakeDone(handshake_done_frame) => {
                Frame::HandshakeDone(handshake_done_frame)
            }
            ReliableFrame::Stream(stream_frame) => Frame::StreamCtl(stream_frame),
        }
    }
}

/// Some modules that need send specific frames can implement `SendFrame` trait directly.
///
/// Alternatively, a temporary buffer that stores certain frames can also implement this trait,
/// But additional processing is required to ensure that the frames in the buffer are eventually
/// sent to the peer.
pub trait SendFrame<T> {
    /// Need send the frames to the peer
    fn send_frame<I: IntoIterator<Item = T>>(&self, iter: I);
}

/// Some modules that need receive specific frames can implement `ReceiveFrame` trait directly.
///
/// Alternatively, a temporary buffer that stores certain frames can also implement this trait,
/// But additional processing is required to ensure that the frames in the buffer are eventually
/// delivered to the corresponding modules.
pub trait ReceiveFrame<T> {
    type Output;

    /// Receive the frames from the peer
    fn recv_frame(&self, frame: &T) -> Result<Self::Output, crate::error::Error>;
}

/// Reads frames from a buffer until the packet buffer is empty.
#[derive(Deref, DerefMut)]
pub struct FrameReader {
    #[deref]
    #[deref_mut]
    payload: Bytes,
    packet_type: Type,
}

impl FrameReader {
    /// Creates a [`FrameReader`] for a packet of type `packet_type`
    pub fn new(payload: Bytes, packet_type: Type) -> Self {
        Self {
            payload,
            packet_type,
        }
    }
}

impl Iterator for FrameReader {
    type Item = Result<(Frame, FrameType), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.payload.is_empty() {
            return None;
        }

        match io::be_frame(&self.payload, self.packet_type) {
            Ok((consumed, frame, frame_type)) => {
                self.payload.advance(consumed);
                Some(Ok((frame, frame_type)))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

impl<T: BufMut> WriteFrame<StreamCtlFrame> for T {
    fn put_frame(&mut self, frame: &StreamCtlFrame) {
        match frame {
            StreamCtlFrame::ResetStream(frame) => self.put_frame(frame),
            StreamCtlFrame::StopSending(frame) => self.put_frame(frame),
            StreamCtlFrame::MaxStreamData(frame) => self.put_frame(frame),
            StreamCtlFrame::MaxStreams(frame) => self.put_frame(frame),
            StreamCtlFrame::StreamDataBlocked(frame) => self.put_frame(frame),
            StreamCtlFrame::StreamsBlocked(frame) => self.put_frame(frame),
        }
    }
}

impl<T: BufMut> WriteFrame<ReliableFrame> for T {
    fn put_frame(&mut self, frame: &ReliableFrame) {
        match frame {
            ReliableFrame::NewToken(frame) => self.put_frame(frame),
            ReliableFrame::MaxData(frame) => self.put_frame(frame),
            ReliableFrame::DataBlocked(frame) => self.put_frame(frame),
            ReliableFrame::NewConnectionId(frame) => self.put_frame(frame),
            ReliableFrame::RetireConnectionId(frame) => self.put_frame(frame),
            ReliableFrame::HandshakeDone(frame) => self.put_frame(frame),
            ReliableFrame::Stream(frame) => self.put_frame(frame),
        }
    }
}

#[cfg(test)]
mod tests {
    use nom::Parser;

    use super::*;
    use crate::{
        packet::{
            PacketContains,
            r#type::{
                Type,
                long::{Type::V1, Ver1},
                short::OneRtt,
            },
        },
        varint::{WriteVarInt, be_varint},
    };

    #[test]
    fn test_frame_type_conversion() {
        let frame_types = vec![
            FrameType::Padding,
            FrameType::Ping,
            FrameType::Ack(0),
            FrameType::Stream(0),
            FrameType::MaxData,
            FrameType::ConnectionClose(0),
            FrameType::HandshakeDone,
            FrameType::Datagram(0),
        ];

        for frame_type in frame_types {
            let byte: VarInt = frame_type.into();
            assert_eq!(FrameType::try_from(byte).unwrap(), frame_type);
        }
    }

    #[test]
    fn test_frame_type_specs() {
        assert!(FrameType::Padding.specs().contain(Spec::NonAckEliciting));
        assert!(
            FrameType::Ack(0)
                .specs()
                .contain(Spec::CongestionControlFree)
        );
        assert!(FrameType::Stream(0).specs().contain(Spec::FlowControlled));
        assert!(FrameType::PathChallenge.specs().contain(Spec::ProbeNewPath));
    }

    #[test]
    fn test_frame_type_belongs_to() {
        let initial = Type::Long(V1(Ver1::INITIAL));
        assert!(FrameType::Padding.belongs_to(initial));
        assert!(FrameType::Ping.belongs_to(initial));
        assert!(FrameType::Ack(0).belongs_to(initial));
        assert!(!FrameType::Stream(0).belongs_to(initial));
    }

    #[test]
    fn test_frame_reader() {
        let mut buf = bytes::BytesMut::new();
        buf.put_u8(0x00); // PADDING
        buf.put_u8(0x01); // PING

        let packet_type = Type::Long(V1(Ver1::INITIAL));
        let mut reader = FrameReader::new(buf.freeze(), packet_type);

        // Read PADDING frame
        let (frame, frame_type) = reader.next().unwrap().unwrap();
        assert!(matches!(frame, Frame::Padding(_)));
        assert!(frame_type.specs().contain(Spec::NonAckEliciting));

        // Read PING frame
        let (frame, frame_type) = reader.next().unwrap().unwrap();
        assert!(matches!(frame, Frame::Ping(_)));
        assert!(!frame_type.specs().contain(Spec::NonAckEliciting));

        // No more frames
        assert!(reader.next().is_none());
    }

    #[test]
    fn test_invalid_frame_type() {
        assert!(FrameType::try_from(VarInt::from_u32(0xFF)).is_err());
    }

    #[test]
    fn test_extetion_frame_type() {
        use crate::varint::WriteVarInt;

        #[derive(Debug, Clone, Eq, PartialEq)]
        struct AddAddressFrame {
            pub seq_num: VarInt,
            pub tire: VarInt,
            pub nat_type: VarInt,
        }

        fn be_add_address_frame(input: &[u8]) -> nom::IResult<&[u8], AddAddressFrame> {
            use nom::{combinator::verify, sequence::preceded};
            preceded(
                verify(be_varint, |typ| typ == &VarInt::from_u32(0xff)),
                (be_varint, be_varint, be_varint),
            )
            .map(|(seq_num, tire, nat_type)| AddAddressFrame {
                seq_num,
                tire,
                nat_type,
            })
            .parse(input)
        }

        fn parse_address_frame(input: &[u8]) -> Result<(usize, AddAddressFrame), Error> {
            let origin = input.len();
            let (remain, frame) = be_add_address_frame(input).map_err(|_| {
                Error::IncompleteType(format!("Incomplete frame type from input: {input:?}"))
            })?;
            let consumed = origin - remain.len();
            Ok((consumed, frame))
        }

        impl<T: bytes::BufMut> super::io::WriteFrame<AddAddressFrame> for T {
            fn put_frame(&mut self, frame: &AddAddressFrame) {
                self.put_varint(&0xff_u32.into());
                self.put_varint(&frame.seq_num);
                self.put_varint(&frame.tire);
                self.put_varint(&frame.nat_type);
            }
        }

        let mut buf = bytes::BytesMut::new();
        let add_address_frame = AddAddressFrame {
            seq_num: VarInt::from_u32(0x01),
            tire: VarInt::from_u32(0x02),
            nat_type: VarInt::from_u32(0x03),
        };
        buf.put_frame(&add_address_frame);
        buf.put_frame(&PaddingFrame);
        buf.put_frame(&PaddingFrame);
        buf.put_frame(&add_address_frame);
        buf.put_varint(&0xfe_u32.into());
        let mut padding_count = 0;
        let mut add_address_count = 0;
        let mut reader = FrameReader::new(buf.freeze(), Type::Short(OneRtt(0.into())));
        loop {
            match reader.next() {
                Some(Ok((frame, typ))) => {
                    assert!(matches!(frame, Frame::Padding(_)));
                    assert_eq!(typ, FrameType::Padding);
                    padding_count += 1;
                }
                Some(Err(_e)) => {
                    // Parse Extension frame
                    if let Ok((consum, frame)) = parse_address_frame(&reader) {
                        reader.advance(consum);
                        assert_eq!(frame, add_address_frame);
                        add_address_count += 1;
                    } else {
                        reader.clear();
                    }
                }
                None => break,
            };
        }
        assert_eq!(padding_count, 2);
        assert_eq!(add_address_count, 2);
    }

    #[test]
    fn test_handless_extension_frame() {
        let mut buf = bytes::BytesMut::new();
        buf.put_frame(&PaddingFrame);
        buf.put_frame(&PaddingFrame);
        // error frame type
        buf.put_varint(&0xfe_u32.into());
        buf.put_frame(&PaddingFrame);

        let mut padding_count = 0;
        let _ = FrameReader::new(buf.freeze(), Type::Short(OneRtt(0.into()))).try_fold(
            PacketContains::default(),
            |packet_contains, frame| {
                let (frame, frame_type) = frame?;

                assert!(matches!(frame, Frame::Padding(_)));
                assert_eq!(frame_type, FrameType::Padding);
                padding_count += 1;
                Result::<_, Error>::Ok(packet_contains)
            },
        );

        assert_eq!(padding_count, 2);
    }
}
