//! `zx_spectrum_machine_plugin` as a loadable library: the entry points `retroclawd` looks for,
//! forwarding to the implementation in the `zx-spectrum-machine-plugin` crate.

plugin_api::export_plugin!(zx_spectrum_machine_plugin_impl::entry_v1);
