//! MongoDB 远程会话的帧队列规则：请求标识、缓冲拼帧、按 `responseTo`
//! 配对 pending、`hello` 前规则、`moreToCome` 免应答与中断清算。
//!
//! 本类型不持有 socket。宿主（N-API / CLI）把收到的字节喂给
//! [`MongoWireSession::push_bytes`]，把 [`MongoWireSession::begin_request`]
//! 返回的帧字节写进 socket；解压与命令编解码仍归 `protocol.mongo`。
//! 语义与线协议规范一致：应答按头里的 `responseTo` 找回请求，
//! 乱序到达的并发响应各自配对，不依赖 FIFO。

use database_protocol_mongo::wire::{OP_MSG, decode_op_msg_command, peek_header};

/// 线协议规范默认的 `maxMessageSizeBytes`。
pub const DEFAULT_MAX_FRAME_SIZE: i64 = 48_000_000;

/// `OP_MSG` 标志位 1：发送方不等待应答（`w: 0` 的 moreToCome 写）。
pub const MORE_TO_COME_FLAG: u32 = 2;

/// 会话规则拒绝请求的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// 连接已中断（坏帧、无主应答或宿主 `fail_all`），会话不再接受新请求。
    Failed {
        /// 中断原因，与 `WireEvent::Abandoned` 的 reason 相同。
        reason: String,
    },
    /// `hello` 握手完成前只允许握手命令。
    NotHandshaked {
        /// 被拒绝的命令名。
        command: String,
    },
    /// 载荷不是可解析的 OP_MSG 命令文档。
    MalformedCommand,
}

impl core::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Failed { reason } => write!(formatter, "mongo session failed: {reason}"),
            Self::NotHandshaked { command } => {
                write!(formatter, "command {command} requires the hello handshake to complete first")
            }
            Self::MalformedCommand => write!(formatter, "mongo session: OP_MSG body is not a decodable command"),
        }
    }
}

/// 一次喂入字节后产生、交给宿主分发的会话事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireEvent {
    /// 一条完整应答帧，已按头里的 `responseTo` 找回对应请求。
    Response {
        /// 原请求的 `requestId`。
        request_id: i32,
        /// 完整应答帧字节（含 16 字节头），解压与解码由宿主继续走 `protocol.mongo`。
        frame: Vec<u8>,
    },
    /// 会话中断：登记等待中的全部请求一次性清算，宿主据此 reject 各自 Promise。
    Abandoned {
        /// 被清算的请求 `requestId`，按登记顺序。
        request_ids: Vec<i32>,
        /// 中断原因。
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expectation {
    /// 必须收到应答。
    Acknowledged,
    /// `moreToCome` 请求：通常无应答，mongod 出错时可能仍回一帧，收到即吸收。
    Optional,
}

#[derive(Debug)]
struct PendingRequest {
    request_id: i32,
    expectation: Expectation,
    handshake: bool,
}

/// 单连接的 MongoDB 帧会话：拼帧、请求标识、pending 配对、握手门禁与中断清算。
#[derive(Debug)]
pub struct MongoWireSession {
    next_request_id: i32,
    buffer: Vec<u8>,
    pending: Vec<PendingRequest>,
    handshaked: bool,
    failed: Option<String>,
    max_frame_size: i64,
}

impl Default for MongoWireSession {
    fn default() -> Self {
        Self::new()
    }
}

impl MongoWireSession {
    /// 以规范默认帧上限创建会话；首个请求的 `requestId` 为 1。
    pub fn new() -> Self {
        Self::with_max_frame_size(DEFAULT_MAX_FRAME_SIZE)
    }

    /// 自定义帧上限创建会话。
    pub fn with_max_frame_size(max_frame_size: i64) -> Self {
        Self { next_request_id: 1, buffer: Vec::new(), pending: Vec::new(), handshaked: false, failed: None, max_frame_size }
    }

    /// `hello` 握手是否已完成（对应握手请求的应答帧到达即置位）。
    pub fn is_handshaked(&self) -> bool {
        self.handshaked
    }

    /// 会话是否仍可发请求（未中断即为真）。
    pub fn is_live(&self) -> bool {
        self.failed.is_none()
    }

    /// 把 OP_MSG 载荷登记为一条请求，返回写往 socket 的完整帧字节。
    ///
    /// `more_to_come` 为真时置 OP_MSG flag 1 且不登记 pending。握手完成前
    /// 只放行 `hello` / `isMaster` 族命令，其余返回 [`SessionError::NotHandshaked`]。
    pub fn begin_request(&mut self, op_msg_body: &[u8], more_to_come: bool) -> Result<Vec<u8>, SessionError> {
        if let Some(reason) = &self.failed {
            return Err(SessionError::Failed { reason: reason.clone() });
        }
        let command = decode_op_msg_command(op_msg_body).map_err(|_| SessionError::MalformedCommand)?;
        let name = command.fields.first().map(|(key, _)| key.clone()).ok_or(SessionError::MalformedCommand)?;
        let handshake = is_handshake_command(&name);
        if !self.handshaked && !handshake {
            return Err(SessionError::NotHandshaked { command: name });
        }

        let request_id = self.take_request_id();
        let mut body = op_msg_body.to_vec();
        if more_to_come {
            if body.len() < 4 {
                return Err(SessionError::MalformedCommand);
            }
            body[..4].copy_from_slice(&MORE_TO_COME_FLAG.to_le_bytes());
            self.pending.push(PendingRequest { request_id, expectation: Expectation::Optional, handshake });
        }
        else {
            self.pending.push(PendingRequest { request_id, expectation: Expectation::Acknowledged, handshake });
        }

        let mut frame = Vec::with_capacity(16 + body.len());
        frame.extend_from_slice(&((16 + body.len()) as u32).to_le_bytes());
        frame.extend_from_slice(&request_id.to_le_bytes());
        frame.extend_from_slice(&0_i32.to_le_bytes());
        frame.extend_from_slice(&OP_MSG.to_le_bytes());
        frame.extend_from_slice(&body);
        Ok(frame)
    }

    /// 喂入 socket 收到的字节，返回本次凑齐的应答帧与中断事件。
    ///
    /// 半帧留在内部缓冲，等后续字节补齐；坏长度或无主应答会清算全部
    /// pending 并把会话置为中断。
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<WireEvent> {
        let mut events = Vec::new();
        if self.failed.is_some() {
            return events;
        }
        self.buffer.extend_from_slice(chunk);
        loop {
            if self.buffer.len() < 4 {
                break;
            }
            let announced = i32::from_le_bytes([self.buffer[0], self.buffer[1], self.buffer[2], self.buffer[3]]) as i64;
            if announced < 16 || announced > self.max_frame_size {
                self.fail(&format!("frame announces an invalid length {announced}"), &mut events);
                return events;
            }
            let needed = announced as usize;
            if self.buffer.len() < needed {
                break;
            }
            let frame = self.buffer.drain(..needed).collect::<Vec<u8>>();
            self.on_frame(frame, &mut events);
            if self.failed.is_some() {
                return events;
            }
        }
        events
    }

    /// 宿主报告 socket 错误 / 关闭：清算 pending 并中断会话。
    /// 已中断或无 pending 时返回 `None`。
    pub fn fail_all(&mut self, reason: &str) -> Option<WireEvent> {
        if self.failed.is_some() {
            return None;
        }
        let mut events = Vec::new();
        self.fail(reason, &mut events);
        events.into_iter().next()
    }

    fn fail(&mut self, reason: &str, events: &mut Vec<WireEvent>) {
        let reason = reason.to_string();
        self.buffer.clear();
        let request_ids = self.pending.drain(..).map(|entry| entry.request_id).collect::<Vec<_>>();
        if !request_ids.is_empty() {
            events.push(WireEvent::Abandoned { request_ids, reason: reason.clone() });
        }
        self.failed = Some(reason);
    }

    fn on_frame(&mut self, frame: Vec<u8>, events: &mut Vec<WireEvent>) {
        let header = match peek_header(&frame) {
            Some(header) => header,
            None => {
                self.fail("frame is shorter than its header", events);
                return;
            }
        };
        let position = self.pending.iter().position(|entry| entry.request_id == header.response_to);
        let Some(position) = position
        else {
            self.fail(&format!("response {response_to} matches no pending request", response_to = header.response_to), events);
            return;
        };
        let entry = self.pending.remove(position);
        if entry.expectation == Expectation::Optional {
            // moreToCome 请求的可选错误应答：吸收，不发事件。
            return;
        }
        if entry.handshake {
            self.handshaked = true;
        }
        events.push(WireEvent::Response { request_id: entry.request_id, frame });
    }

    fn take_request_id(&mut self) -> i32 {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.checked_add(1).unwrap_or(1);
        if self.next_request_id <= 0 {
            self.next_request_id = 1;
        }
        request_id
    }
}

fn is_handshake_command(name: &str) -> bool {
    matches!(name, "hello" | "helloOk" | "isMaster" | "ismaster")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 期望帧按线协议规范手工拼字节，不经被测组帧路径生成载荷之外的字段。
    fn raw_frame(request_id: i32, response_to: i32, opcode: i32, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(&((16 + payload.len()) as u32).to_le_bytes());
        frame.extend_from_slice(&request_id.to_le_bytes());
        frame.extend_from_slice(&response_to.to_le_bytes());
        frame.extend_from_slice(&opcode.to_le_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    /// `hello: 1` 的 OP_MSG 载荷，期望字节按 BSON 规范手工构造：
    /// flags(4)=0、section kind 0x00、文档 `{ hello: 1 }`。
    fn hello_body() -> Vec<u8> {
        let mut document = Vec::new();
        document.extend_from_slice(&16_i32.to_le_bytes()); // 文档总长
        document.push(0x10); // int32
        document.extend_from_slice(b"hello\0");
        document.extend_from_slice(&1_i32.to_le_bytes());
        document.push(0x00); // 文档结束
        let mut body = vec![0_u8; 4];
        body.push(0x00);
        body.extend_from_slice(&document);
        body
    }

    fn ping_body() -> Vec<u8> {
        let mut document = Vec::new();
        document.extend_from_slice(&15_i32.to_le_bytes());
        document.push(0x10);
        document.extend_from_slice(b"ping\0");
        document.extend_from_slice(&1_i32.to_le_bytes());
        document.push(0x00);
        let mut body = vec![0_u8; 4];
        body.push(0x00);
        body.extend_from_slice(&document);
        body
    }

    #[test]
    fn begin_request_frames_the_command_with_increasing_ids() {
        let mut session = MongoWireSession::new();
        let frame = session.begin_request(&hello_body(), false).expect("hello allowed pre-handshake");
        // 期望字节：长度 16+21=37、requestId 1、responseTo 0、opcode 2013、载荷原样。
        let mut expected = Vec::new();
        expected.extend_from_slice(&37_u32.to_le_bytes());
        expected.extend_from_slice(&1_i32.to_le_bytes());
        expected.extend_from_slice(&0_i32.to_le_bytes());
        expected.extend_from_slice(&2013_i32.to_le_bytes());
        expected.extend_from_slice(&hello_body());
        assert_eq!(frame, expected);
        assert!(!session.is_handshaked());

        // 握手未完成时非握手命令被拒。
        let error = session.begin_request(&ping_body(), false).expect_err("ping before hello");
        assert_eq!(error, SessionError::NotHandshaked { command: "ping".into() });

        // 应答到达后握手完成，ping 放行且 id 递增（hello 占 1，被拒的 ping 不占号）。
        let events = session.push_bytes(&raw_frame(9, 1, OP_MSG, b"{ok}"));
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], WireEvent::Response { request_id: 1, frame } if frame == &raw_frame(9, 1, OP_MSG, b"{ok}")));
        assert!(session.is_handshaked());
        session.begin_request(&ping_body(), false).expect("ping after handshake");
        let events = session.push_bytes(&raw_frame(10, 2, OP_MSG, b"{pong}"));
        assert!(matches!(&events[0], WireEvent::Response { request_id: 2, .. }));
    }

    #[test]
    fn partial_frames_are_buffered_until_complete() {
        let mut session = MongoWireSession::new();
        session.begin_request(&hello_body(), false).unwrap();
        let frame = raw_frame(5, 1, OP_MSG, b"{ok}");
        assert!(session.push_bytes(&frame[..7]).is_empty());
        assert!(session.push_bytes(&frame[7..frame.len() - 1]).is_empty());
        let events = session.push_bytes(&[frame[frame.len() - 1]]);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], WireEvent::Response { request_id: 1, .. }));
        // 一次喂入多帧全部拆出。
        session.begin_request(&ping_body(), false).unwrap();
        session.begin_request(&ping_body(), false).unwrap();
        let both = [raw_frame(6, 2, OP_MSG, b"a"), raw_frame(7, 3, OP_MSG, b"bb")].concat();
        let events = session.push_bytes(&both);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], WireEvent::Response { request_id: 2, .. }));
        assert!(matches!(&events[1], WireEvent::Response { request_id: 3, .. }));
    }

    #[test]
    fn concurrent_out_of_order_responses_pair_by_response_to() {
        let mut session = MongoWireSession::new();
        session.begin_request(&hello_body(), false).unwrap();
        session.push_bytes(&raw_frame(1, 1, OP_MSG, b"{hello}"));
        let first = session.begin_request(&ping_body(), false).unwrap();
        let second = session.begin_request(&ping_body(), false).unwrap();
        assert_ne!(first[4..8], second[4..8]);
        // 服务端乱序：先回 requestId 4（对应第二个请求 3）。
        let events = session.push_bytes(&raw_frame(4, 3, OP_MSG, b"{second}"));
        assert!(matches!(&events[0], WireEvent::Response { request_id: 3, frame } if frame == &raw_frame(4, 3, OP_MSG, b"{second}")));
        let events = session.push_bytes(&raw_frame(3, 2, OP_MSG, b"{first}"));
        assert!(matches!(&events[0], WireEvent::Response { request_id: 2, frame } if frame == &raw_frame(3, 2, OP_MSG, b"{first}")));
    }

    #[test]
    fn more_to_come_sets_the_flag_and_expects_no_reply() {
        let mut session = MongoWireSession::new();
        session.begin_request(&hello_body(), false).unwrap();
        session.push_bytes(&raw_frame(1, 1, OP_MSG, b"{hello}"));
        let frame = session.begin_request(&ping_body(), true).expect("moreToCome");
        assert_eq!(&frame[16..20], &MORE_TO_COME_FLAG.to_le_bytes());
        // 无应答：pending 已空，后续正常请求配对不受影响。
        session.begin_request(&ping_body(), false).unwrap();
        let events = session.push_bytes(&raw_frame(3, 3, OP_MSG, b"{pong}"));
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], WireEvent::Response { request_id: 3, .. }));
        // mongod 对 moreToCome 写返回错误应答时静默吸收，不打乱配对。
        session.begin_request(&ping_body(), true).unwrap();
        session.begin_request(&ping_body(), false).unwrap();
        assert!(session.push_bytes(&raw_frame(4, 4, OP_MSG, b"{writeError}")).is_empty());
        let events = session.push_bytes(&raw_frame(5, 5, OP_MSG, b"{pong}"));
        assert_eq!(events, vec![WireEvent::Response { request_id: 5, frame: raw_frame(5, 5, OP_MSG, b"{pong}") }]);
    }

    #[test]
    fn invalid_length_abandons_every_pending_request() {
        let mut session = MongoWireSession::new();
        session.begin_request(&hello_body(), false).unwrap();
        session.push_bytes(&raw_frame(1, 1, OP_MSG, b"{hello}"));
        session.begin_request(&ping_body(), false).unwrap();
        session.begin_request(&ping_body(), false).unwrap();
        let mut lying = Vec::new();
        lying.extend_from_slice(&5_i32.to_le_bytes()); // < 16，非法
        let events = session.push_bytes(&lying);
        assert_eq!(events, vec![WireEvent::Abandoned { request_ids: vec![2, 3], reason: "frame announces an invalid length 5".into() }]);
        assert!(!session.is_live());
        let error = session.begin_request(&ping_body(), false).expect_err("dead session");
        assert!(matches!(error, SessionError::Failed { .. }));
        assert!(session.push_bytes(&raw_frame(9, 2, OP_MSG, b"late")).is_empty());
    }

    #[test]
    fn oversized_frame_announcement_abandons_and_kills_the_session() {
        let mut session = MongoWireSession::with_max_frame_size(64);
        session.begin_request(&hello_body(), false).unwrap();
        let mut header = Vec::new();
        header.extend_from_slice(&100_i32.to_le_bytes()); // 超出自定义上限 64
        let events = session.push_bytes(&header);
        assert!(matches!(&events[0], WireEvent::Abandoned { request_ids, reason } if request_ids == &vec![1] && reason.contains("100")));
        assert!(!session.is_live());
    }

    #[test]
    fn unsolicited_response_abandons_the_rest() {
        let mut session = MongoWireSession::new();
        session.begin_request(&hello_body(), false).unwrap();
        session.push_bytes(&raw_frame(1, 1, OP_MSG, b"{hello}"));
        session.begin_request(&ping_body(), false).unwrap();
        let events = session.push_bytes(&raw_frame(77, 42, OP_MSG, b"{ghost}"));
        assert_eq!(events, vec![WireEvent::Abandoned { request_ids: vec![2], reason: "response 42 matches no pending request".into() }]);
        assert!(!session.is_live());
    }

    #[test]
    fn fail_all_liquidates_once_and_close_is_idempotent() {
        let mut session = MongoWireSession::new();
        session.begin_request(&hello_body(), false).unwrap();
        let event = session.fail_all("connection closed").expect("first failure liquidates");
        assert_eq!(event, WireEvent::Abandoned { request_ids: vec![1], reason: "connection closed".into() });
        assert!(session.fail_all("again").is_none());
        let error = session.begin_request(&hello_body(), false).expect_err("after close");
        assert!(matches!(error, SessionError::Failed { reason } if reason == "connection closed"));
        // 无 pending 时 fail_all 不发事件但仍中断会话。
        let mut fresh = MongoWireSession::new();
        assert!(fresh.fail_all("client closed").is_none());
        assert!(!fresh.is_live());
    }

    #[test]
    fn malformed_bodies_are_rejected_without_consuming_ids() {
        let mut session = MongoWireSession::new();
        let error = session.begin_request(&[0, 0, 0, 0], false).expect_err("truncated body");
        assert_eq!(error, SessionError::MalformedCommand);
        let frame = session.begin_request(&hello_body(), false).expect("hello still fine");
        assert_eq!(&frame[4..8], &1_i32.to_le_bytes());
    }
}
