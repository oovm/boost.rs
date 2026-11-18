//! 请求构造器。

use science_cas::api::{CasRequest, DomainGoal};
use science_types::TermId;

/// `CasRequest::Term`。
pub fn term_request(term: TermId) -> CasRequest {
    CasRequest::Term(term)
}

/// `CasRequest::Goal`。
pub fn goal_request(goal: DomainGoal) -> CasRequest {
    CasRequest::Goal(goal)
}
