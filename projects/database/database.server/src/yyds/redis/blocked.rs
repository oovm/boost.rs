//! 监听器级的阻塞等待表与阻塞命令参数规划。
//!
//! 阻塞命令的数据只能由别的命令写入，解除阻塞的信号（`CLIENT UNBLOCK`）也来自另一条
//! 连接，因此等待状态必须由监听器共享：每个客户端把自己等待的键登记进来，命令完成时
//! 广播一次，等待者醒来重新尝试。队首判定在这里完成，同一键上的等待者按登记顺序被服务。
//!
//! 真正的取数语义仍归 `database_execution` 的集合命令处理函数：这里只按命令形状挑出
//! 「该由我尝试的键」，再用非阻塞语义调用一次，因此不存在第二份 pop 实现。

use std::{
    collections::{HashMap, VecDeque},
    sync::{Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use database_types::cluster::Namespace;

/// 一次阻塞等待的解除方式，对应 `CLIENT UNBLOCK <id> [TIMEOUT|ERROR]`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnblockMode {
    /// 按超时语义返回空回复，连接继续服务。
    Timeout,
    /// 返回 `UNBLOCKED` 错误。
    Error,
}

/// 阻塞命令所属的命令家族，决定重新尝试时走哪个处理函数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    /// `BLPOP` / `BRPOP` / `BLMOVE` / `BRPOPLPUSH` / `BLMPOP`。
    List,
    /// `BZPOPMIN` / `BZPOPMAX` / `BZMPOP`。
    Zset,
}

/// 一条阻塞命令里与等待有关的部分。
#[derive(Debug)]
pub(crate) struct BlockingPlan {
    /// 取数语义归属的命令家族。
    pub(crate) family: Family,
    /// 命令参数里各个键的下标，按命令顺序排列。
    pub(crate) key_indices: Vec<usize>,
    /// `BLMPOP` / `BZMPOP` 的 `numkeys` 下标；其余命令为 `None`。
    pub(crate) numkeys: Option<usize>,
    /// 超时参数原文（浮点秒），`0` 与负数表示无限等待。
    pub(crate) timeout: Vec<u8>,
}

/// 等待线程醒来的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
    /// 可能有新数据到达，带上新的 generation 重新尝试。
    Signaled(u64),
    /// 截止时间先到。
    TimedOut,
    /// `CLIENT UNBLOCK` 选中了这条连接。
    Unblocked(UnblockMode),
}

#[derive(Debug, Default)]
struct WaitState {
    /// 每个逻辑键上等待的客户端 id，按登记顺序排队。
    queues: HashMap<(Namespace, Vec<u8>), VecDeque<u64>>,
    /// 客户端 id → 它等待的键数；出现在表里即表示该连接当前处于阻塞态。
    clients: HashMap<u64, usize>,
    /// `CLIENT UNBLOCK` 选中的客户端，等待线程醒来后取走。
    unblocked: HashMap<u64, UnblockMode>,
    /// 每次可能有新数据或队首变化时前进一格，等待者据此判断是否需要重新尝试。
    generation: u64,
}

/// 监听器级阻塞等待表，由 `ClientRegistry` 持有并与所有连接共享。
#[derive(Debug, Default)]
pub(crate) struct BlockedTable {
    state: Mutex<WaitState>,
    signal: Condvar,
}

impl BlockedTable {
    fn lock(&self) -> MutexGuard<'_, WaitState> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// 让 `generation` 前进一格并唤醒所有等待者：它要么重新尝试取数，要么重新判定队首。
    fn bump(&self, state: &mut WaitState) {
        state.generation = state.generation.wrapping_add(1);
        self.signal.notify_all();
    }

    /// 把一条连接登记到它等待的每个键的队尾，返回去重后的键槽位。
    pub(crate) fn register(&self, client: u64, namespace: &Namespace, keys: &[&[u8]]) -> Vec<(Namespace, Vec<u8>)> {
        let mut state = self.lock();
        let mut slots: Vec<(Namespace, Vec<u8>)> = Vec::with_capacity(keys.len());
        for key in keys {
            let slot = (namespace.clone(), key.to_vec());
            if slots.contains(&slot) {
                continue;
            }
            state.queues.entry(slot.clone()).or_default().push_back(client);
            slots.push(slot);
        }
        state.clients.insert(client, slots.len());
        self.bump(&mut state);
        slots
    }

    /// 注销一条连接的全部等待；队首变化同样要唤醒其余等待者。
    pub(crate) fn release(&self, client: u64, slots: &[(Namespace, Vec<u8>)]) {
        let mut state = self.lock();
        for slot in slots {
            if let Some(queue) = state.queues.get_mut(slot) {
                queue.retain(|queued| *queued != client);
                if queue.is_empty() {
                    state.queues.remove(slot);
                }
            }
        }
        state.clients.remove(&client);
        self.bump(&mut state);
    }

    /// 该客户端在这个键上是否排第一；只有队首有权取元素，FIFO 由此保证。
    pub(crate) fn is_head(&self, client: u64, slot: &(Namespace, Vec<u8>)) -> bool {
        self.lock().queues.get(slot).and_then(|queue| queue.front()).is_some_and(|head| *head == client)
    }

    /// 当前 generation，等待者用它判断醒来后是否值得重新尝试。
    pub(crate) fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// 宣告「数据可能变了」：任何命令结束后调用，等待者据此重新尝试。
    pub(crate) fn wake(&self) {
        self.bump(&mut self.lock());
    }

    /// 睡到截止时间、被解除阻塞或 generation 前进为止。
    pub(crate) fn wait(&self, client: u64, deadline: Option<Instant>, observed: u64) -> Wake {
        let mut state = self.lock();
        loop {
            if let Some(mode) = state.unblocked.remove(&client) {
                return Wake::Unblocked(mode);
            }
            if state.generation != observed {
                return Wake::Signaled(state.generation);
            }
            let Some(deadline) = deadline
            else {
                state = self.signal.wait(state).unwrap_or_else(|poison| poison.into_inner());
                continue;
            };
            let now = Instant::now();
            if now >= deadline {
                return Wake::TimedOut;
            }
            let (guard, _) = self.signal.wait_timeout(state, deadline - now).unwrap_or_else(|poison| poison.into_inner());
            state = guard;
        }
    }

    /// 选中一条正在阻塞的连接并唤醒它，返回它是否真的在阻塞。
    pub(crate) fn unblock(&self, client: u64, mode: UnblockMode) -> bool {
        let mut state = self.lock();
        if !state.clients.contains_key(&client) {
            return false;
        }
        state.unblocked.insert(client, mode);
        self.bump(&mut state);
        true
    }

    /// 当前阻塞的连接数，`INFO clients` 的 `blocked_clients` 使用。
    pub(crate) fn blocked_clients(&self) -> usize {
        self.lock().clients.len()
    }

    /// 当前被阻塞的不同键数，`INFO clients` 的 `total_blocking_keys` 使用。
    pub(crate) fn total_blocking_keys(&self) -> usize {
        self.lock().queues.len()
    }
}

/// 解析一条阻塞命令的等待参数；不是阻塞命令或形状不合法时返回 `None`。
///
/// `None` 表示「交给集合命令处理器按原语义处理」：形状不合法时由它写出正确的 arity /
/// 语法错误，这里不重复判定。
pub(crate) fn plan(command: &[u8], arguments: &[&[u8]]) -> Option<BlockingPlan> {
    let count = arguments.len();
    let plan = |family, key_indices, numkeys, timeout: &[u8]| BlockingPlan { family, key_indices, numkeys, timeout: timeout.to_vec() };
    if command.eq_ignore_ascii_case(b"BLPOP") || command.eq_ignore_ascii_case(b"BRPOP") {
        if count < 3 {
            return None;
        }
        return Some(plan(Family::List, (1..count - 1).collect(), None, arguments[count - 1]));
    }
    if command.eq_ignore_ascii_case(b"BZPOPMIN") || command.eq_ignore_ascii_case(b"BZPOPMAX") {
        if count < 3 {
            return None;
        }
        return Some(plan(Family::Zset, (1..count - 1).collect(), None, arguments[count - 1]));
    }
    if command.eq_ignore_ascii_case(b"BLMOVE") {
        if count != 6 {
            return None;
        }
        return Some(plan(Family::List, vec![1], None, arguments[5]));
    }
    if command.eq_ignore_ascii_case(b"BRPOPLPUSH") {
        if count != 4 {
            return None;
        }
        return Some(plan(Family::List, vec![1], None, arguments[3]));
    }
    if command.eq_ignore_ascii_case(b"BLMPOP") || command.eq_ignore_ascii_case(b"BZMPOP") {
        if count < 5 {
            return None;
        }
        let number = canonical_positive(arguments[2])?;
        let end = 3usize.checked_add(number)?;
        if count < end + 1 {
            return None;
        }
        let family = if command.eq_ignore_ascii_case(b"BLMPOP") { Family::List } else { Family::Zset };
        return Some(plan(family, (3..end).collect(), Some(2), arguments[1]));
    }
    None
}

/// 按「只尝试队首命中的键」重建命令参数。
///
/// `BLMPOP` / `BZMPOP` 的 `numkeys` 要跟着重写，否则处理器会读到多余的键。其余命令的
/// 非键参数原样保留，因此重建后的命令与传入的命令语义相同，只是键列表变短。
pub(crate) fn head_arguments(command: &[u8], arguments: &[&[u8]], plan: &BlockingPlan, heads: &[usize]) -> Vec<Vec<u8>> {
    let mut rebuilt = Vec::with_capacity(arguments.len());
    rebuilt.push(command.to_vec());
    for (index, argument) in arguments.iter().enumerate().skip(1) {
        if Some(index) == plan.numkeys {
            continue;
        }
        if plan.key_indices.contains(&index) && !heads.contains(&index) {
            continue;
        }
        rebuilt.push(argument.to_vec());
    }
    if plan.numkeys.is_some() {
        rebuilt.insert(2, heads.len().to_string().into_bytes());
    }
    rebuilt
}

/// 把阻塞超时（浮点秒）换算成截止时间；`0`、负数与亚毫秒正数按执行层同一口径处理。
///
/// 语法与范围已经由第一次非阻塞尝试校验过（错误由执行层给出），这里只做同样的换算：
/// 非正数为无限等待，正的亚毫秒值按最短 1 毫秒处理。
pub(crate) fn deadline(raw: &[u8]) -> Option<Instant> {
    let seconds: f64 = std::str::from_utf8(raw).ok()?.parse().ok()?;
    let milliseconds = (seconds * 1000.0).trunc();
    if !milliseconds.is_finite() {
        return None;
    }
    let milliseconds = if milliseconds < 1.0 {
        if seconds <= 0.0 {
            return None;
        }
        1
    }
    else {
        milliseconds as u64
    };
    Instant::now().checked_add(Duration::from_millis(milliseconds))
}

/// 规范十进制正整数：只接受能原样往返的写法，`0` 与负数都拒绝。
fn canonical_positive(bytes: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(bytes).ok()?;
    let value: i64 = text.parse().ok()?;
    (value > 0 && value.to_string() == text).then(|| value as usize)
}
