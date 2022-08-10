// We normally wouldn't have to do this, but since most of ic-kit users will build for wasm, we
// should handle this and print a nice compiler error to not confuse the users with 177 errors
// printed on their screen.
cfg_if::cfg_if! {
    if #[cfg(target_family = "wasm")] {
        compile_error!("IC-Kit runtime does not support builds for WASM.");
    } else {
        pub mod call;
        pub mod canister;
        pub mod replica;
        pub mod stable;
        pub mod types;
        pub mod users;
        #[macro_use]
        pub mod macros;

        pub use canister::{Canister, CanisterMethod};
        pub use replica::Replica;
        pub use tokio::runtime::Builder as TokioRuntimeBuilder;

        pub mod prelude {
            pub use crate::canister::Canister;
            pub use crate::replica::Replica;
            pub use crate::types::CanisterId;
            pub use crate::users;
            pub use canister_builder;
        }
    }
}
