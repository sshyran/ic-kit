use crate::{CallResponse, Context};
use ic_cdk;
use ic_cdk::export::candid::utils::{ArgumentDecoder, ArgumentEncoder};
use ic_cdk::export::{candid, Principal};
use std::any::{Any, TypeId};
use std::collections::BTreeMap;

static mut CONTEXT: Option<IcContext> = None;

pub struct IcContext {
    /// The storage for this context.
    storage: BTreeMap<TypeId, Box<dyn Any>>,
}

impl IcContext {
    /// Return a mutable reference to the context.
    #[inline(always)]
    pub fn context() -> &'static mut IcContext {
        unsafe {
            if let Some(ctx) = &mut CONTEXT {
                ctx
            } else {
                CONTEXT = Some(IcContext {
                    storage: BTreeMap::new(),
                });
                IcContext::context()
            }
        }
    }
}

impl Context for IcContext {
    #[inline(always)]
    fn id(&self) -> Principal {
        ic_cdk::id()
    }

    #[inline(always)]
    fn time(&self) -> u64 {
        ic_cdk::api::time()
    }

    #[inline(always)]
    fn balance(&self) -> u64 {
        ic_cdk::api::canister_balance()
    }

    #[inline(always)]
    fn caller(&self) -> Principal {
        ic_cdk::api::caller()
    }

    #[inline(always)]
    fn msg_cycles_available(&self) -> u64 {
        ic_cdk::api::call::msg_cycles_available()
    }

    #[inline(always)]
    fn msg_cycles_accept(&mut self, amount: u64) -> u64 {
        ic_cdk::api::call::msg_cycles_accept(amount)
    }

    #[inline(always)]
    fn msg_cycles_refunded(&self) -> u64 {
        ic_cdk::api::call::msg_cycles_refunded()
    }

    #[inline(always)]
    fn get_mut<T: 'static + Default>(&mut self) -> &mut T {
        let type_id = std::any::TypeId::of::<T>();
        self.storage
            .entry(type_id)
            .or_insert_with(|| Box::new(T::default()))
            .downcast_mut()
            .expect("Unexpected value of invalid type.")
    }

    #[inline(always)]
    fn delete<T: 'static + Default>(&mut self) -> bool {
        let type_id = std::any::TypeId::of::<T>();
        self.storage.remove(&type_id).is_some()
    }

    #[inline(always)]
    fn stable_store<T>(&mut self, data: T) -> Result<(), candid::Error>
    where
        T: ArgumentEncoder,
    {
        ic_cdk::storage::stable_save(data)
    }

    #[inline(always)]
    fn stable_restore<T>(&self) -> Result<T, String>
    where
        T: for<'de> ArgumentDecoder<'de>,
    {
        ic_cdk::storage::stable_restore()
    }

    fn call_raw(
        &'static mut self,
        id: Principal,
        method: &'static str,
        args_raw: Vec<u8>,
        cycles: u64,
    ) -> CallResponse<Vec<u8>> {
        Box::pin(async move { ic_cdk::api::call::call_raw(id, method, args_raw, cycles).await })
    }

    #[inline(always)]
    fn set_certified_data(&mut self, data: &[u8]) {
        ic_cdk::api::set_certified_data(data);
    }

    #[inline(always)]
    fn data_certificate(&self) -> Option<Vec<u8>> {
        ic_cdk::api::data_certificate()
    }
}
