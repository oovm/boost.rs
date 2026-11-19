//! `database-server` owns service sessions, auth ingress, prepared-statement lifecycle and request dispatch.
#![warn(missing_docs)]

/// Embedded engine host service — loopback-first YY wire serve for out-of-process clients.
pub mod yydb;

/// Distributed runtime gateways — MySQL / PostgreSQL / Redis / Mongo disguise protocols.
pub mod yyds;

/// 节点运行时协调 — 运行状态文件、运行代号与停止标记。
pub mod node;
