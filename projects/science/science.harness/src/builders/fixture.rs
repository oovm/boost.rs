//! 基于 `Session` 的 fixture，用于构造类型化 Science 合同。

use science_cas::{CasEngine, Session};
use science_types::{ResultId, TermId};

use crate::builders::{DomainRequestBuilder, TermBuilder};

/// 拥有一个 [`Session`]，用于类型化合同构造与执行。
///
/// 这 **不是** `@sxo/harness`，且不得扩展方言 / 解析 API。
pub struct SessionFixture {
    session: Session,
    engine: CasEngine,
}

impl SessionFixture {
    /// 新 `Session` + engine 句柄。
    pub fn new() -> Self {
        Self { session: Session::new(), engine: CasEngine::new() }
    }

    /// 可变 `Session` 借用。
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// 共享 `Session` 借用。
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 类型化 term 构造器（无具名 head）。
    pub fn terms(&mut self) -> TermBuilder<'_> {
        TermBuilder::new(&mut self.session)
    }

    /// 领域目标辅助。
    pub fn domain(&self) -> DomainRequestBuilder {
        DomainRequestBuilder
    }

    /// 经 engine IR 路径求值 term。执行失败经 [`science_types::Result`] 传播，禁止吞错。
    pub fn evaluate_term(&mut self, term: TermId) -> science_types::Result<TermId> {
        self.engine.evaluate(&mut self.session, term)
    }

    /// 执行中立 [`science_cas::api::CasRequest`]。
    pub fn execute_request(&mut self, request: science_cas::api::CasRequest) -> science_types::Result<ResultId> {
        self.engine.execute_request(&mut self.session, request)
    }

    /// 在 session arena 上做结构相等。
    pub fn structural_eq(&self, a: TermId, b: TermId) -> bool {
        self.session.arena.structural_eq(a, b)
    }
}

impl Default for SessionFixture {
    fn default() -> Self {
        Self::new()
    }
}
