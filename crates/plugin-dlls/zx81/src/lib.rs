//! `zx81_machine_plugin` as a loadable library: the entry points `retroclawd` looks for,
//! forwarding to the implementation in the `zx81-machine-plugin` crate.

plugin_api::export_plugin!(zx81_machine_plugin_impl::entry_v1);
