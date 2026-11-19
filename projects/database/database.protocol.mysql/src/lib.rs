//! MySQL classic 协议单包组帧，不含 SQL 解释。

#![warn(missing_docs)]

/// classic 包头能表示的最大载荷长度。
pub const MAX_PACKET_PAYLOAD: usize = 0x00ff_ffff;

/// classic 协议握手宣告的版本号。
pub const PROTOCOL_VERSION: u8 = 10;

/// 一条借用接收缓冲区二进制载荷的包。
#[derive(Debug, PartialEq, Eq)]
pub struct Packet<'input> {
    /// 序号，由连接状态机校验。
    pub sequence: u8,
    /// 载荷，可能是更大逻辑消息的一个分片。
    pub payload: &'input [u8],
    /// 已消费字节数（含 4 字节头）。
    pub consumed: usize,
}

/// 组帧失败或资源超限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketError {
    /// 载荷超过调用方接收上限或线长范围。
    PayloadTooLarge,
}

/// 解码一条 classic 包，流水线后续数据保持未消费。
/// 最大长度包并不等于完整逻辑消息：连接层必须重组续包并校验序号。
pub fn decode_packet(input: &[u8], max_payload: usize) -> Result<Option<Packet<'_>>, PacketError> {
    if input.len() < 4 {
        return Ok(None);
    }
    let length = usize::from(input[0]) | (usize::from(input[1]) << 8) | (usize::from(input[2]) << 16);
    if length > max_payload {
        return Err(PacketError::PayloadTooLarge);
    }
    let consumed = length + 4;
    if input.len() < consumed {
        return Ok(None);
    }
    Ok(Some(Packet { sequence: input[3], payload: &input[4..consumed], consumed }))
}

/// 编码单包。不会把一条逻辑消息拆成多包。
pub fn encode_packet(sequence: u8, payload: &[u8]) -> Result<Vec<u8>, PacketError> {
    let length = payload_length(payload)?;
    let mut bytes = Vec::with_capacity(length + 4);
    append_packet(&mut bytes, sequence, payload)?;
    Ok(bytes)
}

/// 把一条已经编码好的载荷按 classic 协议组帧追加到缓冲区。
pub fn append_packet(out: &mut Vec<u8>, sequence: u8, payload: &[u8]) -> Result<(), PacketError> {
    let length = payload_length(payload)?;
    out.reserve(length + 4);
    out.extend_from_slice(&[length as u8, (length >> 8) as u8, (length >> 16) as u8, sequence]);
    out.extend_from_slice(payload);
    Ok(())
}

fn payload_length(payload: &[u8]) -> Result<usize, PacketError> {
    if payload.len() > MAX_PACKET_PAYLOAD {
        return Err(PacketError::PayloadTooLarge);
    }
    Ok(payload.len())
}

/// length-encoded 整数解码的错误形态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LenencError {
    /// 载荷在编码值结束前耗尽。
    Truncated,
    /// 首字节是 `0xFF`，它从不作为 length-encoded 值的开头。
    InvalidMarker,
}

/// 解码一个 length-encoded 整数。`Ok(None)` 对应 `0xFB` 的 SQL NULL 标记。
/// `cursor` 在成功时推进到值之后；失败时的推进位置未定义，调用方必须放弃该游标。
pub fn read_length_encoded(payload: &[u8], cursor: &mut usize) -> Result<Option<u64>, LenencError> {
    let &first = payload.get(*cursor).ok_or(LenencError::Truncated)?;
    *cursor += 1;
    match first {
        0x00..=0xfa => Ok(Some(u64::from(first))),
        0xfb => Ok(None),
        0xfc => {
            let bytes = payload.get(*cursor..*cursor + 2).ok_or(LenencError::Truncated)?;
            *cursor += 2;
            Ok(Some(u64::from(u16::from_le_bytes([bytes[0], bytes[1]]))))
        }
        0xfd => {
            let bytes = payload.get(*cursor..*cursor + 3).ok_or(LenencError::Truncated)?;
            *cursor += 3;
            Ok(Some(u64::from(bytes[0]) | (u64::from(bytes[1]) << 8) | (u64::from(bytes[2]) << 16)))
        }
        0xfe => {
            let bytes = payload.get(*cursor..*cursor + 8).ok_or(LenencError::Truncated)?;
            *cursor += 8;
            let mut value = [0u8; 8];
            value.copy_from_slice(bytes);
            Ok(Some(u64::from_le_bytes(value)))
        }
        _ => Err(LenencError::InvalidMarker),
    }
}

/// 把整数写成 MySQL length-encoded integer。
///
/// `251` 不能用单字节 `0xFB`（保留表示 `NULL`），必须走 `0xFC` 双字节形式。
/// `0xFD` 形式只携带 3 字节小端整数，与 [`read_length_encoded`] 的解码宽度一致。
pub fn encode_length_encoded(out: &mut Vec<u8>, value: u64) {
    match value {
        0..=250 => out.push(value as u8),
        251 => out.extend_from_slice(&[252, 251, 0]),
        252..=0xFFFF => {
            out.push(252);
            out.extend_from_slice(&(value as u16).to_le_bytes());
        }
        0x10000..=0xFF_FFFF => {
            out.push(253);
            out.extend_from_slice(&(value as u32).to_le_bytes()[..3]);
        }
        _ => {
            out.push(254);
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

/// `MYSQL_TYPE_*` 线类型码，由参数定义、列定义与二进制行共同携带。
pub const MYSQL_TYPE_DECIMAL: u8 = 0x00;
/// `MYSQL_TYPE_TINY`。
pub const MYSQL_TYPE_TINY: u8 = 0x01;
/// `MYSQL_TYPE_SHORT`。
pub const MYSQL_TYPE_SHORT: u8 = 0x02;
/// `MYSQL_TYPE_LONG`。
pub const MYSQL_TYPE_LONG: u8 = 0x03;
/// `MYSQL_TYPE_FLOAT`。
pub const MYSQL_TYPE_FLOAT: u8 = 0x04;
/// `MYSQL_TYPE_DOUBLE`。
pub const MYSQL_TYPE_DOUBLE: u8 = 0x05;
/// `MYSQL_TYPE_NULL`。
pub const MYSQL_TYPE_NULL: u8 = 0x06;
/// `MYSQL_TYPE_LONGLONG`。
pub const MYSQL_TYPE_LONGLONG: u8 = 0x08;
/// `MYSQL_TYPE_INT24`。
pub const MYSQL_TYPE_INT24: u8 = 0x09;
/// `MYSQL_TYPE_DATE`。
pub const MYSQL_TYPE_DATE: u8 = 0x0A;
/// `MYSQL_TYPE_DATETIME`。
pub const MYSQL_TYPE_DATETIME: u8 = 0x0C;
/// `MYSQL_TYPE_YEAR`。
pub const MYSQL_TYPE_YEAR: u8 = 0x0D;
/// `MYSQL_TYPE_NEWDECIMAL`。
pub const MYSQL_TYPE_NEWDECIMAL: u8 = 0xF6;
/// `MYSQL_TYPE_TINY_BLOB`。
pub const MYSQL_TYPE_TINY_BLOB: u8 = 0xF9;
/// `MYSQL_TYPE_MEDIUM_BLOB`。
pub const MYSQL_TYPE_MEDIUM_BLOB: u8 = 0xFA;
/// `MYSQL_TYPE_LONG_BLOB`。
pub const MYSQL_TYPE_LONG_BLOB: u8 = 0xFB;
/// `MYSQL_TYPE_BLOB`。
pub const MYSQL_TYPE_BLOB: u8 = 0xFC;
/// `MYSQL_TYPE_VAR_STRING`。
pub const MYSQL_TYPE_VAR_STRING: u8 = 0xFD;
/// `MYSQL_TYPE_STRING`。
pub const MYSQL_TYPE_STRING: u8 = 0xFE;

/// OK / EOF 响应携带的服务端状态位：显式事务打开。
pub const SERVER_STATUS_IN_TRANS: u16 = 0x0001;
/// OK / EOF 响应携带的服务端状态位：autocommit 生效。
pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;

/// 编码 OK 载荷：标识字节 + lenenc affected_rows + lenenc last_insert_id + 状态 + 警告。
pub fn ok_payload(affected_rows: u64, last_insert_id: u64, status_flags: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(13);
    payload.push(0x00);
    encode_length_encoded(&mut payload, affected_rows);
    encode_length_encoded(&mut payload, last_insert_id);
    payload.extend_from_slice(&status_flags.to_le_bytes());
    payload.extend_from_slice(&0_u16.to_le_bytes());
    payload
}

/// 编码简化 EOF 载荷：`0xFE` + 警告 0 + 状态位。
pub fn eof_payload(status_flags: u16) -> Vec<u8> {
    let mut payload = [0xFE, 0x00, 0x00, 0x00, 0x00];
    payload[3..].copy_from_slice(&status_flags.to_le_bytes());
    payload.to_vec()
}

/// 编码 ERR 载荷：`0xFF` + 错误号 + `#` + SQLSTATE + 消息。
pub fn error_payload(code: u16, sql_state: &str, message: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(message.len() + 9);
    payload.push(0xff);
    payload.extend_from_slice(&code.to_le_bytes());
    payload.push(b'#');
    payload.extend_from_slice(sql_state.as_bytes());
    payload.extend_from_slice(message.as_bytes());
    payload
}

/// 编码 `COM_STMT_PREPARE` 的 OK 载荷。
pub fn prepare_ok_payload(statement_id: u32, parameter_count: u16, column_count: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(12);
    payload.push(0x00);
    payload.extend_from_slice(&statement_id.to_le_bytes());
    payload.extend_from_slice(&column_count.to_le_bytes());
    payload.extend_from_slice(&parameter_count.to_le_bytes());
    payload.push(0x00); // reserved
    payload.extend_from_slice(&0_u16.to_le_bytes()); // warning count
    payload
}

/// 编码 ColumnDefinition41 载荷。`NOT_NULL = 0x0002` 在列不可空时设置。
///
/// TINY 列的 column_length 设置为 1，等价于 TINYINT(1)，驱动程序据此把 BOOLEAN
/// 当成 0/1。BLOB 列声明 binary 字符集（63），驱动据此把载荷当作原始 octets
/// 而不是文本；其 column_length 由调用方传入的 `blob_column_length` 声明。
pub fn column_definition_payload(name: &str, column_type: u8, nullable: bool, blob_column_length: u32) -> Vec<u8> {
    let mut out = Vec::new();
    encode_length_encoded(&mut out, b"def".len() as u64);
    out.extend_from_slice(b"def");
    encode_length_encoded(&mut out, 0); // schema
    encode_length_encoded(&mut out, 0); // table
    encode_length_encoded(&mut out, 0); // org_table
    encode_length_encoded(&mut out, name.len() as u64);
    out.extend_from_slice(name.as_bytes());
    encode_length_encoded(&mut out, 0); // org_name
    out.push(0x0C); // fixed length
    let charset: u16 = if column_type == MYSQL_TYPE_BLOB { 63 } else { 0x000C };
    out.extend_from_slice(&charset.to_le_bytes()); // charset
    let column_length: u32 = match column_type {
        MYSQL_TYPE_TINY => 1,
        MYSQL_TYPE_BLOB => blob_column_length,
        _ => 255,
    };
    out.extend_from_slice(&column_length.to_le_bytes());
    out.push(column_type);
    let flags: u16 = if !nullable { 0x0002 } else { 0 };
    out.extend_from_slice(&flags.to_le_bytes());
    out.push(if column_type == MYSQL_TYPE_DOUBLE { 31 } else { 0 }); // decimals
    out.extend_from_slice(&[0u8; 2]); // filler
    out
}
