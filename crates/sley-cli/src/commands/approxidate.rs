//! Thin re-export shim over the canonical approxidate port, which lives in
//! [`sley_core::date::approxidate`] (dependency-free, so rev can share the
//! same parser without a presentation-tier dependency). Every historical
//! `crate::commands::approxidate::*` call site keeps working unchanged.

pub(crate) use crate::sley_core::date::approxidate::{
    format_expiry_date, parse_approxidate, parse_commit_date, parse_expiry_date,
};
