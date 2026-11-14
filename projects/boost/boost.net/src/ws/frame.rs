//! WebSocket frame encoding and decoding (RFC 6455 §5).

use std::io::{Read, Write};

use crate::NetError;

pub(crate) const OP_CONTINUATION: u8 = 0x0;
pub(crate) const OP_TEXT: u8 = 0x1;
pub(crate) const OP_BINARY: u8 = 0x2;
pub(crate) const OP_CLOSE: u8 = 0x8;
pub(crate) const OP_PING: u8 = 0x9;
pub(crate) const OP_PONG: u8 = 0xA;

/// One decoded frame.
pub(crate) struct Frame {
    pub(crate) fin: bool,
    pub(crate) opcode: u8,
    pub(crate) payload: Vec<u8>,
}

/// Write one masked client frame.
pub(crate) fn write<W: Write>(writer: &mut W, opcode: u8, fin: bool, payload: &[u8], mask: [u8; 4]) -> Result<(), NetError> {
    let mut header = Vec::with_capacity(14);
    header.push(if fin { 0x80 | opcode } else { opcode });
    let length = payload.len();
    if length < 126 {
        header.push(0x80 | length as u8);
    }
    else if let Ok(length) = u16::try_from(length) {
        header.push(0x80 | 126);
        header.extend_from_slice(&length.to_be_bytes());
    }
    else {
        header.push(0x80 | 127);
        header.extend_from_slice(&(length as u64).to_be_bytes());
    }
    header.extend_from_slice(&mask);
    writer.write_all(&header).map_err(NetError::Body)?;
    let mut masked = Vec::with_capacity(length);
    masked.extend(payload.iter().enumerate().map(|(index, byte)| byte ^ mask[index % 4]));
    writer.write_all(&masked).map_err(NetError::Body)?;
    writer.flush().map_err(NetError::Body)
}

/// Read one server frame, rejecting masked or reserved frames.
pub(crate) fn read<R: Read>(reader: &mut R, max_payload: usize) -> Result<Frame, NetError> {
    let mut head = [0u8; 2];
    reader.read_exact(&mut head).map_err(NetError::Body)?;
    if head[0] & 0x70 != 0 {
        return Err(NetError::Protocol("reserved frame bits must be zero".into()));
    }
    let fin = head[0] & 0x80 != 0;
    let opcode = head[0] & 0x0F;
    if head[1] & 0x80 != 0 {
        return Err(NetError::Protocol("server frames must not be masked".into()));
    }
    let length = match head[1] & 0x7F {
        126 => {
            let mut bytes = [0u8; 2];
            reader.read_exact(&mut bytes).map_err(NetError::Body)?;
            u64::from(u16::from_be_bytes(bytes))
        }
        127 => {
            let mut bytes = [0u8; 8];
            reader.read_exact(&mut bytes).map_err(NetError::Body)?;
            let length = u64::from_be_bytes(bytes);
            if length & 0x8000_0000_0000_0000 != 0 {
                return Err(NetError::Protocol("frame payload length has the reserved high bit set".into()));
            }
            length
        }
        value => u64::from(value),
    };
    let control = matches!(opcode, OP_CLOSE | OP_PING | OP_PONG);
    if control {
        if !fin {
            return Err(NetError::Protocol("control frames must not be fragmented".into()));
        }
        if length > 125 {
            return Err(NetError::Protocol("control frame payload exceeds 125 bytes".into()));
        }
    }
    let length = usize::try_from(length).map_err(|_| NetError::Protocol("frame payload length overflows usize".into()))?;
    if length > max_payload {
        return Err(NetError::BodyTooLarge { limit: max_payload });
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).map_err(NetError::Body)?;
    Ok(Frame { fin, opcode, payload })
}
