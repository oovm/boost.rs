//! Redis 认证语义：`requirepass` 判定、`AUTH` 凭据校验与 `NOAUTH` 命令门禁。
//!
//! 口令本身是服务器级配置（`CONFIG SET requirepass` 可在线改），由宿主传入；这里拥有
//! 的是每条连接的认证状态机：未认证时哪些命令可以运行、`AUTH` 的两种形态如何校验凭据、
//! 失败回哪条错误文案。监听器网关与嵌入式会话共用这套判定，宿主只负责转发与记账。

use database_protocol_redis::codec::{CommandError, Reply};

/// 未认证时仍允许运行的命令，与真机 ACL `@connection` 的最小子集一致。
const UNAUTHENTICATED_COMMANDS: &[&[u8]] = &[b"AUTH", b"HELLO", b"QUIT", b"RESET"];

/// 一条连接的认证状态。
///
/// `authenticated` 在连接建立时按当时是否配置了口令初始化：没有口令就没有认证门槛。
/// `requirepass` 每次判定都从服务器级配置读取，`CONFIG SET requirepass` 之后新建的
/// 连接立即生效，已认证的旧连接不受影响，与真机一致。
#[derive(Debug, Clone)]
pub struct AuthState {
    authenticated: bool,
}

impl AuthState {
    /// 按连接建立时刻的口令配置初始化。
    pub fn new(requirepass: Option<&str>) -> Self {
        Self { authenticated: requirepass.is_none() }
    }

    /// 当前是否已通过认证。
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// 未认证时返回该命令的 `NOAUTH` 错误回复，已认证或命令属于免认证集合时返回 `None`。
    pub fn gate(&self, command: &[u8], requirepass: Option<&str>) -> Option<Reply> {
        if self.authenticated || requirepass.is_none() || UNAUTHENTICATED_COMMANDS.iter().any(|allowed| command.eq_ignore_ascii_case(allowed)) {
            return None;
        }
        Some(CommandError::Coded("NOAUTH Authentication required.".to_owned()).to_reply())
    }

    /// 执行 `AUTH [username] password`。
    ///
    /// 两参数形是旧版单口令形，三参数形带用户名。校验通过后连接进入已认证状态。
    pub fn authenticate(&mut self, arguments: &[&[u8]], requirepass: Option<&str>) -> Reply {
        let legacy = arguments.len() == 2;
        if !matches!(arguments.len(), 2 | 3) {
            return CommandError::Failed("wrong number of arguments for 'auth' command".to_owned()).to_reply();
        }
        let username = if legacy { "default".to_owned() } else { String::from_utf8_lossy(arguments[1]).into_owned() };
        let password = String::from_utf8_lossy(arguments[if legacy { 1 } else { 2 }]).into_owned();
        match self.check_credential(&username, &password, legacy, requirepass) {
            Ok(()) => Reply::ok(),
            Err(error) => error.to_reply(),
        }
    }

    /// 校验一组凭据，供 `AUTH` 与 `HELLO ... AUTH` 共用。
    ///
    /// 成功即把连接标记为已认证；失败返回真机口径的错误：未配置口令时的旧版单口令形
    /// 回配置提示文案，其余回 `WRONGPASS`。
    pub fn check_credential(&mut self, username: &str, password: &str, legacy: bool, requirepass: Option<&str>) -> Result<(), CommandError> {
        if credential_matches(requirepass, username, password, legacy) {
            self.authenticated = true;
            return Ok(());
        }
        Err(credential_error(requirepass, username, legacy))
    }
}

/// 凭据是否成立。逐字节比较走常量时间路径，避免口令长度与内容通过耗时泄露。
fn credential_matches(requirepass: Option<&str>, username: &str, password: &str, legacy: bool) -> bool {
    match requirepass {
        None => !legacy && username == "default",
        Some(configured) => username == "default" && constant_time_eq(configured.as_bytes(), password.as_bytes()),
    }
}

/// 校验失败时真机回的错误文案。
fn credential_error(requirepass: Option<&str>, username: &str, legacy: bool) -> CommandError {
    if requirepass.is_none() && legacy {
        return CommandError::Failed(
            "AUTH <password> called without any password configured for the default user. Are you sure your configuration is correct?"
                .to_owned(),
        );
    }
    let _ = username;
    CommandError::Coded("WRONGPASS invalid username-password pair or user is disabled.".to_owned())
}

/// 长度相同才逐字节比较，长度差异本身不泄露内容（真机同样先比长度）。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_only_blocks_when_a_password_is_configured() {
        let state = AuthState::new(Some("secret"));
        assert!(state.gate(b"GET", Some("secret")).is_some());
        assert!(state.gate(b"AUTH", Some("secret")).is_none());
        assert!(state.gate(b"HELLO", Some("secret")).is_none());
        assert!(state.gate(b"QUIT", Some("secret")).is_none());
        assert!(state.gate(b"RESET", Some("secret")).is_none());
        let open = AuthState::new(None);
        assert!(open.gate(b"GET", None).is_none());
    }

    #[test]
    fn authenticate_accepts_the_configured_default_password() {
        let mut state = AuthState::new(Some("secret"));
        let arguments: Vec<&[u8]> = vec![b"AUTH", b"secret"];
        assert_eq!(state.authenticate(&arguments, Some("secret")), Reply::ok());
        assert!(state.is_authenticated());
        assert!(state.gate(b"GET", Some("secret")).is_none());
    }

    #[test]
    fn authenticate_rejects_wrong_credentials() {
        let mut state = AuthState::new(Some("secret"));
        let arguments: Vec<&[u8]> = vec![b"AUTH", b"nope"];
        assert_eq!(
            state.authenticate(&arguments, Some("secret")),
            CommandError::Coded("WRONGPASS invalid username-password pair or user is disabled.".to_owned()).to_reply()
        );
        assert!(!state.is_authenticated());
    }

    #[test]
    fn legacy_form_without_a_configured_password_reports_the_configuration_hint() {
        let mut state = AuthState::new(None);
        let arguments: Vec<&[u8]> = vec![b"AUTH", b"whatever"];
        assert_eq!(
            state.authenticate(&arguments, None),
            CommandError::Failed(
                "AUTH <password> called without any password configured for the default user. Are you sure your configuration is correct?"
                    .to_owned()
            )
            .to_reply()
        );
        // 没有口令时连接本来就没有认证门槛，失败提示不改变可用性。
        assert!(state.gate(b"GET", None).is_none());
    }

    #[test]
    fn named_user_without_a_configured_password_is_wrongpass() {
        let mut state = AuthState::new(None);
        let arguments: Vec<&[u8]> = vec![b"AUTH", b"alice", b"pw"];
        assert_eq!(
            state.authenticate(&arguments, None),
            CommandError::Coded("WRONGPASS invalid username-password pair or user is disabled.".to_owned()).to_reply()
        );
        let arguments: Vec<&[u8]> = vec![b"AUTH", b"default", b"pw"];
        assert_eq!(state.authenticate(&arguments, None), Reply::ok());
    }

    #[test]
    fn arity_is_checked_before_credentials() {
        let mut state = AuthState::new(Some("secret"));
        let arguments: Vec<&[u8]> = vec![b"AUTH"];
        assert_eq!(
            state.authenticate(&arguments, Some("secret")),
            CommandError::Failed("wrong number of arguments for 'auth' command".to_owned()).to_reply()
        );
    }
}
