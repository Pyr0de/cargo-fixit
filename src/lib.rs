#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![warn(clippy::print_stderr)]
#![warn(clippy::print_stdout)]

mod errors;
mod ops;
mod utils;

pub use errors::*;
pub use ops::*;
pub(crate) use utils::format_package_id;

#[doc = include_str!("../README.md")]
#[cfg(doctest)]
pub struct ReadmeDoctests;
