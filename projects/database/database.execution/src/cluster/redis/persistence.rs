//! Redis 持久化与生命周期命令语义：`SAVE` / `BGSAVE` / `LASTSAVE` / `BGREWRITEAOF` / `SHUTDOWN`。
//!
//! 落盘动作本身属于宿主存储（flush 与 fsync 由绑定层执行），这里拥有的是命令状态机：
//! 参数校验、后台保存的进行中标记、`LASTSAVE` 的时间基准与 `SHUTDOWN` 修饰符的互斥规则。
//! 计划返回的 [`PersistenceAction`] 告诉宿主在发出回复前后要做什么，宿主不再自行判定。

use database_protocol_redis::codec::{CommandError, Reply};

/// 宿主在计划之后必须对存储执行的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PersistenceAction {
    /// 无需动作。
    #[default]
    None,
    /// 同步 flush 加 fsync，然后调用 [`PersistenceState::mark_saved`]。
    Save,
    /// 派生后台 flush 加 fsync，完成后调用 [`PersistenceState::finish_background`]。
    BackgroundSave,
    /// 关闭服务器；除 `NOSAVE` 外先按 [`Self::Save`] 语义落盘。
    Shutdown {
        /// 是否在关闭前同步落盘（`NOSAVE` 为 false）。
        persist: bool,
    },
}

/// 一条持久化命令的计划结果。
#[derive(Debug)]
pub struct PersistenceOutcome {
    /// 要发给客户端的回复；`None` 表示无回复（`SHUTDOWN`）。
    pub reply: Option<Reply>,
    /// 宿主需要执行的动作。
    pub action: PersistenceAction,
}

/// 服务器级持久化状态，与真机一样由同一存储的所有连接共享。
#[derive(Debug)]
pub struct PersistenceState {
    last_save: Option<i64>,
    saving: bool,
    started_unix: i64,
}

impl PersistenceState {
    /// 以服务器启动时刻（Unix 秒）初始化；首次保存前 `LASTSAVE` 回这个时刻。
    pub fn new(started_unix: i64) -> Self {
        Self { last_save: None, saving: false, started_unix }
    }

    /// 宿主在服务器真正启动时校正时间基准。
    pub fn set_started(&mut self, started_unix: i64) {
        self.started_unix = started_unix;
    }

    /// 记录一次完成的保存。
    pub fn mark_saved(&mut self, now_unix: i64) {
        self.last_save = Some(now_unix);
    }

    /// `MULTI` 排队期的轻量校验：只查形状，不推进状态。真机把 `BGSAVE` /
    /// `LASTSAVE` / `BGREWRITEAOF` 当作可排队命令，参数错误在入队时就拒绝。
    pub fn validate_for_queue(command: &[u8], arguments: &[&[u8]]) -> Result<(), CommandError> {
        if command.eq_ignore_ascii_case(b"BGSAVE") {
            if arguments.len() == 2 && !arguments[1].eq_ignore_ascii_case(b"SCHEDULE") {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            if arguments.len() > 2 {
                return Err(CommandError::Failed("syntax error".to_owned()));
            }
            return Ok(());
        }
        if arguments.len() != 1 {
            return Err(CommandError::Failed(format!("wrong number of arguments for '{}' command", String::from_utf8_lossy(command).to_lowercase())));
        }
        Ok(())
    }

    /// 后台保存结束：清进行中标记并记录保存时刻。
    pub fn finish_background(&mut self, now_unix: i64) {
        self.saving = false;
        self.last_save = Some(now_unix);
    }

    /// 是否有后台保存进行中。
    pub fn is_saving(&self) -> bool {
        self.saving
    }

    /// 该命令名是否属于持久化与生命周期族。
    pub fn is_command(command: &[u8]) -> bool {
        [b"SAVE".as_slice(), b"BGSAVE", b"LASTSAVE", b"BGREWRITEAOF", b"SHUTDOWN"].iter().any(|name| command.eq_ignore_ascii_case(name))
    }

    /// 规划一条持久化命令：校验参数、推进状态并给出回复与宿主动作。
    pub fn plan(&mut self, command: &[u8], arguments: &[&[u8]], now_unix: i64) -> PersistenceOutcome {
        if command.eq_ignore_ascii_case(b"SAVE") {
            if arguments.len() != 1 {
                return error_outcome(CommandError::Failed("wrong number of arguments for 'save' command".to_owned()));
            }
            if self.saving {
                return error_outcome(CommandError::Failed("Background save already in progress".to_owned()));
            }
            return PersistenceOutcome { reply: Some(Reply::ok()), action: PersistenceAction::Save };
        }
        if command.eq_ignore_ascii_case(b"BGSAVE") {
            if arguments.len() == 2 && !arguments[1].eq_ignore_ascii_case(b"SCHEDULE") {
                return error_outcome(CommandError::Failed("syntax error".to_owned()));
            }
            if arguments.len() > 2 {
                return error_outcome(CommandError::Failed("syntax error".to_owned()));
            }
            // 真机里已有一个 RDB 子进程时两种形态都被拒绝，只有 AOF 子进程才让
            // SCHEDULE 退化为排队；本实现只跟踪 RDB 保存。
            if self.saving {
                return error_outcome(CommandError::Failed("Background save already in progress".to_owned()));
            }
            self.saving = true;
            return PersistenceOutcome {
                reply: Some(Reply::bulk("Background saving started".to_owned())),
                action: PersistenceAction::BackgroundSave,
            };
        }
        if command.eq_ignore_ascii_case(b"LASTSAVE") {
            if arguments.len() != 1 {
                return error_outcome(CommandError::Failed("wrong number of arguments for 'lastsave' command".to_owned()));
            }
            // 真机认为启动即已保存，首次保存之前回服务器启动时刻。
            return PersistenceOutcome { reply: Some(Reply::Integer(self.last_save.unwrap_or(self.started_unix))), action: PersistenceAction::None };
        }
        if command.eq_ignore_ascii_case(b"BGREWRITEAOF") {
            if arguments.len() != 1 {
                return error_outcome(CommandError::Failed("wrong number of arguments for 'bgrewriteaof' command".to_owned()));
            }
            // 真机无论 appendonly 是否开启都启动重写并回 started 状态。
            return PersistenceOutcome { reply: Some(Reply::bulk("Background append only file rewriting started".to_owned())), action: PersistenceAction::None };
        }
        if command.eq_ignore_ascii_case(b"SHUTDOWN") {
            return plan_shutdown(arguments, now_unix);
        }
        error_outcome(CommandError::Failed(format!("wrong number of arguments for '{}' command", String::from_utf8_lossy(command).to_lowercase())))
    }
}

/// `SHUTDOWN` 修饰符集合，与真机命令表一致。
const SHUTDOWN_FLAGS: &[&str] = &["nosave", "save", "now", "force", "abort"];

fn plan_shutdown(arguments: &[&[u8]], _now_unix: i64) -> PersistenceOutcome {
    let mut nosave = false;
    let mut save = false;
    let mut abort = false;
    let mut other = false;
    for argument in &arguments[1..] {
        let flag = String::from_utf8_lossy(argument).to_lowercase();
        if !SHUTDOWN_FLAGS.contains(&flag.as_str()) {
            return error_outcome(CommandError::Failed("syntax error".to_owned()));
        }
        match flag.as_str() {
            "nosave" => nosave = true,
            "save" => save = true,
            "abort" => abort = true,
            _ => other = true,
        }
    }
    // ABORT 与任何其他修饰符互斥，两个保存选择器也互斥，真机都回语法错误。
    if (abort && (nosave || save || other)) || (nosave && save) {
        return error_outcome(CommandError::Failed("syntax error".to_owned()));
    }
    if abort {
        // 本实现一步关闭，永远没有可取消的关闭过程。
        return error_outcome(CommandError::Failed("No shutdown in progress.".to_owned()));
    }
    // 不带 NOSAVE 的关闭先落盘，与真机一致。
    PersistenceOutcome { reply: None, action: PersistenceAction::Shutdown { persist: !nosave } }
}

fn error_outcome(error: CommandError) -> PersistenceOutcome {
    PersistenceOutcome { reply: Some(error.to_reply()), action: PersistenceAction::None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(names: &[&str]) -> Vec<Vec<u8>> {
        names.iter().map(|name| name.as_bytes().to_vec()).collect()
    }

    fn borrow(arguments: &[Vec<u8>]) -> Vec<&[u8]> {
        arguments.iter().map(|argument| argument.as_slice()).collect()
    }

    #[test]
    fn save_reports_progress_while_a_background_save_runs() {
        let mut state = PersistenceState::new(1000);
        let request = arguments(&["BGSAVE"]);
        let outcome = state.plan(b"BGSAVE", &borrow(&request), 1001);
        assert_eq!(outcome.action, PersistenceAction::BackgroundSave);
        assert!(state.is_saving());
        let request = arguments(&["SAVE"]);
        let outcome = state.plan(b"SAVE", &borrow(&request), 1002);
        assert_eq!(outcome.reply, Some(CommandError::Failed("Background save already in progress".to_owned()).to_reply()));
        state.finish_background(1003);
        assert_eq!(state.last_save, Some(1003));
    }

    #[test]
    fn bgsave_only_accepts_schedule() {
        let mut state = PersistenceState::new(1000);
        let request = arguments(&["BGSAVE", "SCHEDULE"]);
        assert_eq!(state.plan(b"BGSAVE", &borrow(&request), 1001).action, PersistenceAction::BackgroundSave);
        state.finish_background(1002);
        let request = arguments(&["BGSAVE", "SCHEDULED"]);
        assert_eq!(state.plan(b"BGSAVE", &borrow(&request), 1003).reply, Some(CommandError::Failed("syntax error".to_owned()).to_reply()));
        let request = arguments(&["BGSAVE", "SCHEDULE", "EXTRA"]);
        assert_eq!(state.plan(b"BGSAVE", &borrow(&request), 1004).reply, Some(CommandError::Failed("syntax error".to_owned()).to_reply()));
    }

    #[test]
    fn lastsave_falls_back_to_the_startup_moment() {
        let mut state = PersistenceState::new(1000);
        let request = arguments(&["LASTSAVE"]);
        assert_eq!(state.plan(b"LASTSAVE", &borrow(&request), 1001).reply, Some(Reply::Integer(1000)));
        state.mark_saved(1002);
        assert_eq!(state.plan(b"LASTSAVE", &borrow(&request), 1003).reply, Some(Reply::Integer(1002)));
    }

    #[test]
    fn shutdown_flags_are_exclusive_and_abort_never_finds_a_process() {
        let mut state = PersistenceState::new(1000);
        let request = arguments(&["SHUTDOWN", "NOSAVE", "SAVE"]);
        assert_eq!(state.plan(b"SHUTDOWN", &borrow(&request), 1001).reply, Some(CommandError::Failed("syntax error".to_owned()).to_reply()));
        let request = arguments(&["SHUTDOWN", "ABORT", "NOW"]);
        assert_eq!(state.plan(b"SHUTDOWN", &borrow(&request), 1001).reply, Some(CommandError::Failed("syntax error".to_owned()).to_reply()));
        let request = arguments(&["SHUTDOWN", "ABORT"]);
        assert_eq!(state.plan(b"SHUTDOWN", &borrow(&request), 1001).reply, Some(CommandError::Failed("No shutdown in progress.".to_owned()).to_reply()));
        let request = arguments(&["SHUTDOWN", "NOSAVE"]);
        let outcome = state.plan(b"SHUTDOWN", &borrow(&request), 1001);
        assert_eq!(outcome.action, PersistenceAction::Shutdown { persist: false });
        assert_eq!(outcome.reply, None);
        let request = arguments(&["SHUTDOWN"]);
        assert_eq!(state.plan(b"SHUTDOWN", &borrow(&request), 1001).action, PersistenceAction::Shutdown { persist: true });
    }
}
