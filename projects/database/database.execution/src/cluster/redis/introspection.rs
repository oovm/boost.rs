//! Redis 自省命令语义：`WAIT` 与 `LATENCY`。
//!
//! 单实例没有副本、没有延迟事件，依赖这些子系统的回答塌缩成真机在同样场景下的形状。
//! `WAIT` 的挂起时长由计划返回，宿主负责停车与超时回复；判定与文案只在这里维护。
//! `SLOWLOG` 与 `MEMORY` 依赖宿主进程级观测，暂未归位。

use database_protocol_redis::codec::{CommandError, Reply};

use super::session::parse_integer;

/// `WAIT numreplicas timeout` 的计划结果。
#[derive(Debug, PartialEq, Eq)]
pub enum WaitPlan {
    /// 立即回复（`numreplicas <= 0` 无需等待）。
    Immediate(Reply),
    /// 挂起连接 `timeout_ms` 毫秒后回复整数 0；0 表示无限等待，与真机一致。
    Delay {
        /// 挂起毫秒数，0 表示无限等待。
        timeout_ms: i64,
    },
}

/// 规划 `WAIT`。单实例永远没有副本可确认，回答的计数恒为 0。
pub fn plan_wait(arguments: &[&[u8]]) -> Result<WaitPlan, CommandError> {
    if arguments.len() != 3 {
        return Err(CommandError::Failed("wrong number of arguments for 'wait' command".to_owned()));
    }
    let Some(numreplicas) = parse_integer(arguments[1])
    else {
        return Err(CommandError::Failed("value is not an integer or out of range".to_owned()));
    };
    let Some(timeout) = parse_integer(arguments[2])
    else {
        return Err(CommandError::Failed("timeout is not an integer or out of range".to_owned()));
    };
    if timeout < 0 {
        return Err(CommandError::Failed("timeout is negative".to_owned()));
    }
    if numreplicas <= 0 {
        return Ok(WaitPlan::Immediate(Reply::Integer(0)));
    }
    Ok(WaitPlan::Delay { timeout_ms: timeout })
}

/// 真机在延迟监控从未开启时回答的报告。
const LATENCY_DOCTOR_DISABLED: &str = concat!(
    "I'm sorry, Dave, I can't do that. Latency monitoring is disabled in this Redis instance. ",
    "You may use \"CONFIG SET latency-monitor-threshold <milliseconds>.\" in order to enable it. ",
    "If we weren't in a deep space mission I'd suggest to take a look at ",
    "https://redis.io/docs/latest/operate/oss_and_stack/management/optimization/latency-monitor.\n",
);

const LATENCY_HELP: &[&str] = &[
    "LATENCY <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
    "DOCTOR",
    "    Return a human readable latency analysis report.",
    "GRAPH <event>",
    "    Return an ASCII latency graph for the <event> class.",
    "HISTORY <event>",
    "    Return time-latency samples for the <event> class.",
    "LATEST",
    "    Return the latest latency samples for all events.",
    "RESET [<event> ...]",
    "    Reset latency data of one or more <event> classes.",
    "    (default: reset all data for all event classes)",
    "HISTOGRAM [COMMAND ...]",
    "    Return a cumulative distribution of latencies in the format of a histogram for the specified command names.",
    "    If no commands are specified then all histograms are replied.",
    "HELP",
    "    Print this help.",
];

/// 规划 `LATENCY` 家族。`Err` 是排队期就要拒绝并中毒事务的校验错误；`Ok` 是运行结果
/// （本服务器不记录延迟事件：`LATEST` 与 `HISTORY` 回空数组，`RESET` 回清空的条数
/// （恒 0），`GRAPH` 按请求事件回真机的无样本错误）。
pub fn plan_latency(arguments: &[&[u8]]) -> Result<Reply, CommandError> {
    if arguments.len() < 2 {
        return Err(CommandError::Failed("wrong number of arguments for 'latency' command".to_owned()));
    }
    let typed = String::from_utf8_lossy(arguments[1]).into_owned();
    let subcommand = typed.to_lowercase();
    if subcommand == "latest" && arguments.len() == 2 {
        return Ok(Reply::array(Vec::new()));
    }
    if subcommand == "history" && arguments.len() == 3 {
        return Ok(Reply::array(Vec::new()));
    }
    if subcommand == "graph" && arguments.len() == 3 {
        let event = String::from_utf8_lossy(arguments[2]).into_owned();
        return Ok(CommandError::Coded(format!("ERR No samples available for event '{event}'")).to_reply());
    }
    if subcommand == "doctor" && arguments.len() == 2 {
        return Ok(Reply::bulk(LATENCY_DOCTOR_DISABLED.to_owned()));
    }
    if subcommand == "reset" && arguments.len() >= 2 {
        return Ok(Reply::Integer(0));
    }
    if subcommand == "histogram" && arguments.len() >= 2 {
        return Ok(Reply::Map(Vec::new()));
    }
    if subcommand == "help" && arguments.len() == 2 {
        return Ok(Reply::array(LATENCY_HELP.iter().map(|line| Reply::bulk((*line).to_owned()))));
    }
    Err(subcommand_syntax_error("LATENCY", &typed))
}

/// 真机把子命令按原样回显，截断到 128 字节。
pub fn subcommand_syntax_error(command: &str, typed: &str) -> CommandError {
    CommandError::Coded(format!(
        "ERR Unknown subcommand or wrong number of arguments for '{}'. Try {command} HELP.",
        typed.chars().take(128).collect::<String>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_without_replicas_never_parks() {
        let arguments: Vec<&[u8]> = vec![b"WAIT", b"0", b"100"];
        assert_eq!(plan_wait(&arguments), Ok(WaitPlan::Immediate(Reply::Integer(0))));
        let arguments: Vec<&[u8]> = vec![b"WAIT", b"-1", b"100"];
        assert_eq!(plan_wait(&arguments), Ok(WaitPlan::Immediate(Reply::Integer(0))));
        let arguments: Vec<&[u8]> = vec![b"WAIT", b"1", b"0"];
        assert_eq!(plan_wait(&arguments), Ok(WaitPlan::Delay { timeout_ms: 0 }));
        let arguments: Vec<&[u8]> = vec![b"WAIT", b"1", b"50"];
        assert_eq!(plan_wait(&arguments), Ok(WaitPlan::Delay { timeout_ms: 50 }));
    }

    #[test]
    fn wait_reports_the_upstream_validation_errors() {
        let arguments: Vec<&[u8]> = vec![b"WAIT", b"1"];
        assert_eq!(plan_wait(&arguments), Err(CommandError::Failed("wrong number of arguments for 'wait' command".to_owned())));
        let arguments: Vec<&[u8]> = vec![b"WAIT", b"x", b"1"];
        assert_eq!(plan_wait(&arguments), Err(CommandError::Failed("value is not an integer or out of range".to_owned())));
        let arguments: Vec<&[u8]> = vec![b"WAIT", b"1", b"x"];
        assert_eq!(plan_wait(&arguments), Err(CommandError::Failed("timeout is not an integer or out of range".to_owned())));
        let arguments: Vec<&[u8]> = vec![b"WAIT", b"1", b"-5"];
        assert_eq!(plan_wait(&arguments), Err(CommandError::Failed("timeout is negative".to_owned())));
    }

    #[test]
    fn latency_collapses_to_the_no_events_shapes() {
        let arguments: Vec<&[u8]> = vec![b"LATENCY", b"LATEST"];
        assert_eq!(plan_latency(&arguments), Ok(Reply::array(Vec::new())));
        let arguments: Vec<&[u8]> = vec![b"LATENCY", b"HISTORY", b"command"];
        assert_eq!(plan_latency(&arguments), Ok(Reply::array(Vec::new())));
        let arguments: Vec<&[u8]> = vec![b"LATENCY", b"GRAPH", b"command"];
        assert_eq!(plan_latency(&arguments), Ok(CommandError::Coded("ERR No samples available for event 'command'".to_owned()).to_reply()));
        let arguments: Vec<&[u8]> = vec![b"LATENCY", b"RESET"];
        assert_eq!(plan_latency(&arguments), Ok(Reply::Integer(0)));
        let arguments: Vec<&[u8]> = vec![b"LATENCY"];
        assert_eq!(plan_latency(&arguments), Err(CommandError::Failed("wrong number of arguments for 'latency' command".to_owned())));
        let arguments: Vec<&[u8]> = vec![b"LATENCY", b"BOGUS"];
        assert_eq!(plan_latency(&arguments), Err(subcommand_syntax_error("LATENCY", "BOGUS")));
    }
}
