//! The provider contract as WIT: `wit/dform-provider.wit` (package
//! `dform:provider`) and `wit/function/dform-function.wit` (package
//! `dform:function`), DESIGN.org R-7 and R-13. Bindings only: no host, no
//! component.
//!
//! [`host`] is the wasmtime side: the `provider-component` and `functions`
//! worlds as types a host instantiates. [`guest`] is the wit-bindgen side:
//! what a provider or function component implements, exported with
//! `guest::provider::export!` or `guest::function::export!`.

#[cfg(feature = "host")]
pub mod host {
    pub mod provider {
        wasmtime::component::bindgen!({
            path: "../../wit",
            world: "dform:provider/provider-component",
        });
    }

    pub mod function {
        wasmtime::component::bindgen!({
            path: "../../wit/function",
            world: "dform:function/functions",
        });
    }
}

#[cfg(feature = "guest")]
pub mod guest {
    pub mod provider {
        wit_bindgen::generate!({
            path: "../../wit",
            world: "dform:provider/provider-component",
            generate_all,
            pub_export_macro: true,
            export_macro_name: "export",
        });
    }

    pub mod function {
        wit_bindgen::generate!({
            path: "../../wit/function",
            world: "dform:function/functions",
            pub_export_macro: true,
            export_macro_name: "export",
        });
    }
}
