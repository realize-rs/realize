pub mod client;
mod convert;
pub mod server;

#[allow(dead_code)]
#[allow(unknown_lints)]
#[allow(clippy::uninlined_format_args)]
#[allow(clippy::extra_unused_type_parameters)]
pub mod control_capnp {
    include!(concat!(env!("OUT_DIR"), "/rpc/control_capnp.rs"));
}
