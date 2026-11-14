//! Correctness tests, cases, fixtures, assertions, and structured results.

pub mod assert;
pub mod case;
pub mod context;
pub mod descriptor;
pub mod fixture;
pub mod report;
pub mod result;
pub mod runner;

pub use crate::{assert, assert_eq, assert_ne, fail};
pub use boost_harness_macro::{case, fixture, test};

pub use crate::test::{
    case::{CaseError, CaseId},
    context::TestContext,
    descriptor::{Capability, Expectation, SourceLocation, TestDescriptor},
    fixture::{Fixture, FixtureDescriptor, IntoTeardown},
    report::render_text,
    result::{AssertionKind, FailureKind, IntoTestFailure, Operand, OperandValue, Phase, TestFailure, TestOutcome, TestResult},
    runner::{execute, execute_with_fixture, run, run_with_fixture},
};
