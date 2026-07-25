use std::{error, fmt};

#[cfg(not(target_family = "wasm"))]
mod real {
    use std::{
        error,
        ffi::{CStr, CString},
        fmt,
        mem::{self, MaybeUninit},
        os::raw::c_char,
    };

    pub use wasmtime_c_api::wasmtime;

    use crate::{FREE_FN, Language, LanguageError, Parser, ffi};

    // Force Cargo to include wasmtime-c-api as a dependency of this crate,
    // even though it is only used by the C code.
    #[allow(unused)]
    fn _use_wasmtime() {
        wasmtime_c_api::wasm_engine_new();
    }

    #[repr(C)]
    #[derive(Clone)]
    #[allow(non_camel_case_types)]
    pub struct wasm_engine_t {
        pub(crate) engine: wasmtime::Engine,
    }

    pub struct WasmStore(*mut ffi::TSWasmStore);

    unsafe impl Send for WasmStore {}
    unsafe impl Sync for WasmStore {}

    #[derive(Debug, PartialEq, Eq)]
    pub struct WasmError {
        pub kind: WasmErrorKind,
        pub message: String,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum WasmErrorKind {
        Parse,
        Compile,
        Instantiate,
        Other,
    }

    impl WasmStore {
        pub fn new(engine: &wasmtime::Engine) -> Result<Self, WasmError> {
            unsafe {
                let mut error = MaybeUninit::<ffi::TSWasmError>::uninit();
                let store = ffi::ts_wasm_store_new(
                    std::ptr::from_ref::<wasmtime::Engine>(engine)
                        .cast_mut()
                        .cast(),
                    error.as_mut_ptr(),
                );
                if store.is_null() {
                    Err(WasmError::new(error.assume_init()))
                } else {
                    Ok(Self(store))
                }
            }
        }

        pub fn load_language(&mut self, name: &str, bytes: &[u8]) -> Result<Language, WasmError> {
            let name = CString::new(name).unwrap();
            unsafe {
                let mut error = MaybeUninit::<ffi::TSWasmError>::uninit();
                let language = ffi::ts_wasm_store_load_language(
                    self.0,
                    name.as_ptr(),
                    bytes.as_ptr().cast::<c_char>(),
                    bytes.len() as u32,
                    error.as_mut_ptr(),
                );
                if language.is_null() {
                    Err(WasmError::new(error.assume_init()))
                } else {
                    Ok(Language(language))
                }
            }
        }

        #[must_use]
        pub fn language_count(&self) -> usize {
            unsafe { ffi::ts_wasm_store_language_count(self.0) }
        }
    }

    impl WasmError {
        unsafe fn new(error: ffi::TSWasmError) -> Self {
            let message = CStr::from_ptr(error.message).to_str().unwrap().to_string();
            (FREE_FN)(error.message.cast());
            Self {
                kind: match error.kind {
                    ffi::TSWasmErrorKindParse => WasmErrorKind::Parse,
                    ffi::TSWasmErrorKindCompile => WasmErrorKind::Compile,
                    ffi::TSWasmErrorKindInstantiate => WasmErrorKind::Instantiate,
                    _ => WasmErrorKind::Other,
                },
                message,
            }
        }
    }

    impl Language {
        #[must_use]
        pub fn is_wasm(&self) -> bool {
            unsafe { ffi::ts_language_is_wasm(self.0) }
        }
    }

    impl Parser {
        pub fn set_wasm_store(&mut self, store: WasmStore) -> Result<(), LanguageError> {
            unsafe { ffi::ts_parser_set_wasm_store(self.0.as_ptr(), store.0) };
            mem::forget(store);
            Ok(())
        }

        pub fn take_wasm_store(&mut self) -> Option<WasmStore> {
            let ptr = unsafe { ffi::ts_parser_take_wasm_store(self.0.as_ptr()) };
            if ptr.is_null() {
                None
            } else {
                Some(WasmStore(ptr))
            }
        }
    }

    impl Drop for WasmStore {
        fn drop(&mut self) {
            unsafe { ffi::ts_wasm_store_delete(self.0) };
        }
    }

    impl fmt::Display for WasmError {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let kind = match self.kind {
                WasmErrorKind::Parse => "Failed to parse Wasm",
                WasmErrorKind::Compile => "Failed to compile Wasm",
                WasmErrorKind::Instantiate => "Failed to instantiate Wasm module",
                WasmErrorKind::Other => "Unknown error",
            };
            write!(f, "{kind}: {}", self.message)
        }
    }

    impl error::Error for WasmError {}
}

#[cfg(not(target_family = "wasm"))]
pub use real::*;

#[cfg(target_family = "wasm")]
mod stub {
    use std::{error, fmt};

    pub mod wasmtime {
        #[derive(Debug)]
        pub struct Config;

        impl Config {
            pub fn new() -> Self {
                Self
            }
        }

        #[derive(Debug)]
        pub struct Engine;

        impl Engine {
            pub fn new(_: &Config) -> Result<Self, String> {
                Ok(Self)
            }
        }
    }

    #[derive(Debug)]
    pub struct WasmStore;

    impl WasmStore {
        pub fn new(_engine: &wasmtime::Engine) -> Result<Self, WasmError> {
            Ok(Self)
        }

        pub fn load_language(
            &mut self,
            _name: &str,
            _bytes: &[u8],
        ) -> Result<crate::Language, WasmError> {
            Err(WasmError {
                kind: WasmErrorKind::Other,
                message: "WASM grammars are not supported in the browser".to_string(),
            })
        }

        #[must_use]
        pub fn language_count(&self) -> usize {
            0
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    pub struct WasmError {
        pub kind: WasmErrorKind,
        pub message: String,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum WasmErrorKind {
        Parse,
        Compile,
        Instantiate,
        Other,
    }

    impl WasmError {
        fn _message(&self) -> &str {
            &self.message
        }
    }

    impl fmt::Display for WasmError {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let kind = match self.kind {
                WasmErrorKind::Parse => "Failed to parse Wasm",
                WasmErrorKind::Compile => "Failed to compile Wasm",
                WasmErrorKind::Instantiate => "Failed to instantiate Wasm module",
                WasmErrorKind::Other => "Unknown error",
            };
            write!(f, "{kind}: {}", self.message)
        }
    }

    impl error::Error for WasmError {}

    impl crate::Language {
        #[must_use]
        pub fn is_wasm(&self) -> bool {
            false
        }
    }

    impl crate::Parser {
        pub fn set_wasm_store(&mut self, _store: WasmStore) -> Result<(), crate::LanguageError> {
            Ok(())
        }

        pub fn take_wasm_store(&mut self) -> Option<WasmStore> {
            Some(WasmStore)
        }
    }
}

#[cfg(target_family = "wasm")]
pub use stub::*;
