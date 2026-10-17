//! A signer as a component of `world signer-component` (`component`):
//! [`signer!`](crate::signer) exports a [`Signer`] as the WIT's `signer`,
//! and, built for wasm, this crate is the component's whole runtime with
//! no std: wasi-libc's allocator, string functions and task hook, which
//! import nothing, so the component imports nothing at all.

use crate::{Command, Got, Material, Need, Outcome, Recipe, Request, Response, Signer, TlsClient};
use alloc::string::String;
use alloc::vec::Vec;

pub mod bindings {
    wit_bindgen::generate!({
        path: "../../wit/host",
        world: "dform:host/signer-component",
        pub_export_macro: true,
        default_bindings_module: "dform_signer::component::bindings",
    });
}

use bindings::exports::dform::host::signer as w;

/// Export `$signer`, an expression of a type that is a [`Signer`], as
/// the component's `signer`.
#[macro_export]
macro_rules! signer {
    ($signer:expr) => {
        struct __DformSigner;

        impl $crate::component::bindings::exports::dform::host::signer::Guest for __DformSigner {
            fn needs() -> ::core::result::Result<
                ::alloc::vec::Vec<$crate::component::bindings::exports::dform::host::signer::Need>,
                ::alloc::string::String,
            > {
                $crate::component::needs(&$signer)
            }

            fn step(
                got: ::alloc::vec::Vec<$crate::component::bindings::exports::dform::host::signer::Got>,
            ) -> ::core::result::Result<
                $crate::component::bindings::exports::dform::host::signer::Outcome,
                ::alloc::string::String,
            > {
                $crate::component::step(&$signer, got)
            }

            fn sign(
                req: $crate::component::bindings::exports::dform::host::signer::Request,
                material: $crate::component::bindings::exports::dform::host::signer::Material,
                now: u64,
            ) -> ::core::result::Result<
                $crate::component::bindings::exports::dform::host::signer::Request,
                ::alloc::string::String,
            > {
                $crate::component::sign(&$signer, req, material, now)
            }

            fn refresh(
                material: $crate::component::bindings::exports::dform::host::signer::Material,
                now: u64,
            ) -> ::core::result::Result<
                $crate::component::bindings::exports::dform::host::signer::Outcome,
                ::alloc::string::String,
            > {
                $crate::component::refresh(&$signer, material, now)
            }
        }

        $crate::component::bindings::export!(__DformSigner with_types_in $crate::component::bindings);
    };
}

pub fn needs(s: &impl Signer) -> Result<Vec<w::Need>, String> {
    Ok(s.needs()?.into_iter().map(Into::into).collect())
}

pub fn step(s: &impl Signer, got: Vec<w::Got>) -> Result<w::Outcome, String> {
    let got: Vec<Got> = got.into_iter().map(Into::into).collect();
    s.step(&got).map(Into::into)
}

pub fn sign(
    s: &impl Signer,
    req: w::Request,
    material: w::Material,
    now: u64,
) -> Result<w::Request, String> {
    s.sign(req.into(), &material.into(), now).map(Into::into)
}

pub fn refresh(s: &impl Signer, material: w::Material, now: u64) -> Result<w::Outcome, String> {
    s.refresh(&material.into(), now).map(Into::into)
}

impl From<w::Request> for Request {
    fn from(r: w::Request) -> Request {
        Request {
            method: r.method,
            url: r.url,
            headers: r.headers,
            body: r.body,
        }
    }
}

impl From<Request> for w::Request {
    fn from(r: Request) -> w::Request {
        w::Request {
            method: r.method,
            url: r.url,
            headers: r.headers,
            body: r.body,
        }
    }
}

impl From<Need> for w::Need {
    fn from(n: Need) -> w::Need {
        match n {
            Need::Env(vars) => w::Need::Env(vars),
            Need::File(path) => w::Need::File(path),
            Need::Exec(Command { argv, env }) => w::Need::Exec(w::Command { argv, env }),
            Need::Fetch(r) => w::Need::Fetch(r.into()),
            Need::Given => w::Need::Given,
        }
    }
}

impl From<w::Got> for Got {
    fn from(g: w::Got) -> Got {
        match g {
            w::Got::Env(vars) => Got::Env(vars),
            w::Got::File(bytes) => Got::File(bytes),
            w::Got::Exec(out) => Got::Exec(out),
            w::Got::Fetch(r) => Got::Fetch(Response {
                status: r.status,
                headers: r.headers,
                body: r.body,
                at: r.at,
            }),
            w::Got::Given(values) => Got::Given(values),
        }
    }
}

impl From<w::Recipe> for Recipe {
    fn from(r: w::Recipe) -> Recipe {
        match r {
            w::Recipe::Bearer(t) => Recipe::Bearer(t),
            w::Recipe::Headers(h) => Recipe::Headers(h),
            w::Recipe::Signed => Recipe::Signed,
            w::Recipe::TlsClient(t) => Recipe::TlsClient(TlsClient {
                cert: t.cert,
                key: t.key,
            }),
            w::Recipe::Proxy(p) => Recipe::Proxy(p),
            w::Recipe::Ca(pem) => Recipe::Ca(pem),
            w::Recipe::Insecure => Recipe::Insecure,
            w::Recipe::ServerName(n) => Recipe::ServerName(n),
        }
    }
}

impl From<Recipe> for w::Recipe {
    fn from(r: Recipe) -> w::Recipe {
        match r {
            Recipe::Bearer(t) => w::Recipe::Bearer(t),
            Recipe::Headers(h) => w::Recipe::Headers(h),
            Recipe::Signed => w::Recipe::Signed,
            Recipe::TlsClient(t) => w::Recipe::TlsClient(w::TlsClient {
                cert: t.cert,
                key: t.key,
            }),
            Recipe::Proxy(p) => w::Recipe::Proxy(p),
            Recipe::Ca(pem) => w::Recipe::Ca(pem),
            Recipe::Insecure => w::Recipe::Insecure,
            Recipe::ServerName(n) => w::Recipe::ServerName(n),
        }
    }
}

impl From<w::Material> for Material {
    fn from(m: w::Material) -> Material {
        Material {
            bytes: m.bytes,
            recipe: m.recipe.into_iter().map(Into::into).collect(),
            expires: m.expires,
            endpoint: m.endpoint,
            namespace: m.namespace,
        }
    }
}

impl From<Material> for w::Material {
    fn from(m: Material) -> w::Material {
        w::Material {
            bytes: m.bytes,
            recipe: m.recipe.into_iter().map(Into::into).collect(),
            expires: m.expires,
            endpoint: m.endpoint,
            namespace: m.namespace,
        }
    }
}

impl From<Outcome> for w::Outcome {
    fn from(o: Outcome) -> w::Outcome {
        match o {
            Outcome::More(needs) => w::Outcome::More(needs.into_iter().map(Into::into).collect()),
            Outcome::Material(m) => w::Outcome::Material(m.into()),
        }
    }
}

/// The component's runtime, with no std: wasi-libc's allocator and the
/// task hook wasm32-wasip3's start-up calls (its string and memory
/// functions import nothing), and a panic that traps.
#[cfg(target_family = "wasm")]
mod runtime {
    use core::alloc::{GlobalAlloc, Layout};

    #[link(name = "c")]
    unsafe extern "C" {
        fn malloc(size: usize) -> *mut u8;
        fn aligned_alloc(align: usize, size: usize) -> *mut u8;
        fn realloc(ptr: *mut u8, size: usize) -> *mut u8;
        fn free(ptr: *mut u8);
    }

    /// What `malloc` aligns to on wasm32.
    const MALLOC_ALIGN: usize = 8;

    /// wasi-libc's heap, which its own task hook allocates from too.
    struct Libc;

    // SAFETY: each call is libc's own for the same layout: an alignment
    // `malloc` does not give goes through `aligned_alloc`, and a block is
    // freed or reallocated by the allocator that gave it (libc's `free`
    // and `realloc` take either).
    unsafe impl GlobalAlloc for Libc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            unsafe {
                match layout.align() <= MALLOC_ALIGN {
                    true => malloc(layout.size()),
                    false => aligned_alloc(layout.align(), layout.size()),
                }
            }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, _: Layout) {
            unsafe { free(ptr) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            unsafe {
                if layout.align() <= MALLOC_ALIGN {
                    return realloc(ptr, size);
                }
                let new = aligned_alloc(layout.align(), size);
                if !new.is_null() {
                    core::ptr::copy_nonoverlapping(ptr, new, layout.size().min(size));
                    free(ptr);
                }
                new
            }
        }
    }

    #[global_allocator]
    static LIBC: Libc = Libc;

    #[panic_handler]
    fn panic(_: &core::panic::PanicInfo) -> ! {
        core::arch::wasm32::unreachable()
    }

    /// The canonical ABI's allocator, which the host calls to place a
    /// call's arguments (the start-up names it).
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn cabi_realloc(
        old: *mut u8,
        old_len: usize,
        align: usize,
        new_len: usize,
    ) -> *mut u8 {
        // SAFETY: the canonical ABI's contract: `old` is null or a block
        // of `old_len` bytes given with `align`.
        unsafe {
            let layout = Layout::from_size_align_unchecked(old_len, align);
            match old.is_null() {
                true => LIBC.alloc(Layout::from_size_align_unchecked(new_len, align)),
                false => LIBC.realloc(old, layout, new_len),
            }
        }
    }
}
