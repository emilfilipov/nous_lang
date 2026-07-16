//! One module per `lullaby` subcommand. Each exposes a single entry point that
//! consumes the already-parsed [`Invocation`](crate::args::Invocation) fields it
//! needs and returns the CLI's `Result<(), String>`.

pub(crate) mod build;
pub(crate) mod fmt;
pub(crate) mod inspect;
pub(crate) mod native;
pub(crate) mod native_link;
pub(crate) mod project;
pub(crate) mod run;
pub(crate) mod test;
pub(crate) mod test_isolate;
pub(crate) mod wasm;
