use crate::types::{
    CanisterId, CanisterReply, EntryMode, Env, IncomingRequestId, Message, OutgoingRequestId,
    RejectionCode, RequestId,
};
use futures::executor::block_on;
use ic_kit_sys::ic0;
use ic_kit_sys::ic0::runtime;
use ic_kit_sys::ic0::runtime::Ic0CallHandlerProxy;
use ic_types::Principal;
use std::any::Any;
use std::cmp::min;
use std::collections::HashMap;
use std::panic::{catch_unwind, RefUnwindSafe};
use std::thread::JoinHandle;
use thread_local_panic_hook::set_hook;
use tokio::select;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::oneshot;

/// A canister that is being executed.
pub struct Canister {
    /// The id of the canister.
    canister_id: Vec<u8>,
    /// The canister balance.
    balance: u128,
    /// Maps the name of each of exported methods to the task function.
    symbol_table: HashMap<String, Box<dyn Fn() + Send + RefUnwindSafe>>,
    /// Map each incoming request id the response we're generating for it.
    msg_reply_data: HashMap<IncomingRequestId, Vec<u8>>,
    /// Map each incoming request to its response channel, if it is None, it means the
    /// message has already been responded to.
    msg_reply_senders: HashMap<IncomingRequestId, oneshot::Sender<CanisterReply>>,
    /// The amount of available cycles for each incoming request. This is only used
    /// for recovering self.env state for reply callbacks.
    cycles_available_store: HashMap<IncomingRequestId, u128>,
    /// Amount of cycles accept during this message process.
    cycles_accepted: u128,
    /// Map each of the out going requests done by this canister to the callbacks for that
    /// call.
    outgoing_calls: HashMap<OutgoingRequestId, RequestCallbacks>,
    /// The canister execution environment.
    env: Env,
    /// The request id of the current incoming message.
    request_id: Option<IncomingRequestId>,
    /// The thread in which the canister is being executed at.
    execution_thread_handle: JoinHandle<()>,
    /// The communication channel to send tasks to the execution thread.
    task_tx: Sender<Box<dyn Fn() + Send + RefUnwindSafe>>,
    /// Emits when the task we just sent has returned.
    task_completion_rx: Receiver<Completion>,
    /// To send the response to the calls.
    reply_tx: Sender<runtime::Response>,
    /// The channel that we use to get the requests from the execution thread.
    request_rx: Receiver<runtime::Request>,
}

#[derive(Debug)]
enum Completion {
    Ok,
    Panicked(String),
}

/// Any of the reply, reject or clean up callbacks.
/// (callback_fun, callback_env)
///
/// The callback_fun can be set to -1 for one-way calls.
type Callback = (isize, isize);

/// The callbacks
struct RequestCallbacks {
    /// The original top-level message which caused this inter-canister call, this is used so
    /// for example when `ic0::msg_reply` is called, we know which call to respond to.
    message_id: IncomingRequestId,
    /// The reply callback that must be called for a reply.
    reply: Callback,
    /// The reject callback that must be called for a reject.
    reject: Callback,
    /// An optional cleanup callback.
    cleanup: Option<Callback>,
}

/// A method exported by the canister.
pub trait CanisterMethod {
    /// The export name of this method, this is the name that the method is
    /// exported by in the WASM binary, examples could be:
    /// - `canister_init`
    /// - `canister_update increment`
    /// - `canister_pre_upgrade`
    ///
    /// See:
    /// https://internetcomputer.org/docs/current/references/ic-interface-spec/#entry-points
    const EXPORT_NAME: &'static str;

    /// The method which is exported by the canister in the WASM, since the entry points
    /// should have a type `() -> ()`, we wrap the canister methods in a function in which
    /// we perform the serialization/deserialization of arguments/responses, using the runtime
    /// primitives.
    fn exported_method();
}

impl Canister {
    pub fn new(canister_id: CanisterId) -> Self {
        let (request_tx, request_rx) = mpsc::channel(8);
        let (reply_tx, reply_rx) = mpsc::channel(8);
        let (task_tx, mut task_rx) = mpsc::channel::<Box<dyn Fn() + Send + RefUnwindSafe>>(8);
        let (task_completion_tx, task_completion_rx) = mpsc::channel(8);

        let execution_thread_handle = std::thread::spawn(move || {
            // Register the ic-kit-sys handler for current thread, this will make ic-kit-sys to
            // forward all of the system calls done in the current thread to the provided channel
            // and use the rx channel for waiting on responses.
            let handle = runtime::RuntimeHandle::new(reply_rx, request_tx);
            ic0::register_handler(handle);

            // set the custom panic hook for this thread, this will give us:
            // - No message such as "thread panic during test" in the terminal.
            // - TODO: Capture the panic location.
            // let panic_hook_tx = task_completion_tx.clone();
            set_hook(Box::new(move |m| {}));

            block_on(async {
                while let Some(task) = task_rx.recv().await {
                    let c = if let Err(payload) = catch_unwind(|| {
                        task();
                    }) {
                        Completion::Panicked(downcast_panic_payload(&payload))
                    } else {
                        Completion::Ok
                    };

                    // In case we panic the hook might have already sent the proper panic message,
                    // and we may be double sending this signal here, but this is okay since,
                    // process_message always makes sure there is no pending signals in this channel
                    // before sending a new task.
                    task_completion_tx.send(c)
                        .await
                        .expect("ic-kit-runtime: Execution thread could not send task-completion signal to the main thread.");
                }
            });
        });

        Self {
            canister_id: Vec::from(Principal::from(canister_id).as_slice()),
            balance: 100_000_000_000_000,
            symbol_table: HashMap::new(),
            msg_reply_data: HashMap::new(),
            msg_reply_senders: HashMap::new(),
            cycles_available_store: HashMap::new(),
            cycles_accepted: 0,
            outgoing_calls: HashMap::new(),
            env: Env::default(),
            request_id: None,
            execution_thread_handle,
            task_tx,
            task_completion_rx,
            reply_tx,
            request_rx,
        }
    }

    /// Provide the canister with the definition of the given method.
    pub fn with_method<M: CanisterMethod + 'static>(mut self) -> Self {
        let method_name = String::from(M::EXPORT_NAME);
        let task_fn = Box::new(M::exported_method);

        if self.symbol_table.contains_key(&method_name) {
            panic!("The canister already has a '{}' method.", method_name);
        }

        self.symbol_table.insert(method_name, task_fn);
        self
    }

    /// Set the canister's cycle balance to this number.
    pub fn with_balance(mut self, balance: u128) -> Self {
        self.balance = balance;
        self
    }

    pub async fn process_message(&mut self, _message: Message) {
        // make sure we clean the task_returned receiver. since we may have sent more than one
        // completion signal from previous task.
        while let Ok(_) = self.task_completion_rx.try_recv() {}

        self.task_tx
            .send(Box::new(|| {
                println!("Some function related to the request.")
            }))
            .await
            .unwrap_or_else(|_| {
                panic!("ic-kit-runtime: Could not send the task to the execution thread.")
            });

        loop {
            select! {
                Some(_c) = self.task_completion_rx.recv() => {
                    // Okay the task returned successfully, we can give up.
                    return;
                },
                Some(req) = self.request_rx.recv() => {
                    let res = req.proxy(self);
                    self.reply_tx
                        .send(res)
                        .await
                        .expect("ic-kit-runtime: Could not send the system API call's response to the execution thread.");
                }
            }
        }
    }
}

impl Ic0CallHandlerProxy for Canister {
    fn msg_arg_data_size(&mut self) -> Result<isize, String> {
        match self.env.entry_mode {
            EntryMode::CustomTask
            | EntryMode::Init
            | EntryMode::Update
            | EntryMode::Query
            | EntryMode::ReplyCallback
            | EntryMode::InspectMessage => Ok(self.env.args.len() as isize),
            _ => Err(format!(
                "msg_arg_data_size can not be called from '{}'",
                self.env.get_entry_point_name()
            )),
        }
    }

    fn msg_arg_data_copy(&mut self, dst: isize, offset: isize, size: isize) -> Result<(), String> {
        match self.env.entry_mode {
            EntryMode::CustomTask
            | EntryMode::Init
            | EntryMode::PostUpgrade
            | EntryMode::Update
            | EntryMode::Query
            | EntryMode::ReplyCallback
            | EntryMode::InspectMessage => {
                let data = self.env.args.as_slice();
                copy_to_canister(dst, offset, size, &data)?;
                Ok(())
            }
            _ => Err(format!(
                "msg_arg_data_copy can not be called from '{}'",
                self.env.get_entry_point_name()
            )),
        }
    }

    fn msg_caller_size(&mut self) -> Result<isize, String> {
        match self.env.entry_mode {
            EntryMode::CustomTask
            | EntryMode::Init
            | EntryMode::PostUpgrade
            | EntryMode::PreUpgrade
            | EntryMode::Update
            | EntryMode::Query
            | EntryMode::InspectMessage => Ok(self.env.sender.as_slice().len() as isize),
            _ => Err(format!(
                "msg_caller_size can not be called from '{}'",
                self.env.get_entry_point_name()
            )),
        }
    }

    fn msg_caller_copy(&mut self, dst: isize, offset: isize, size: isize) -> Result<(), String> {
        match self.env.entry_mode {
            EntryMode::CustomTask
            | EntryMode::Init
            | EntryMode::PostUpgrade
            | EntryMode::PreUpgrade
            | EntryMode::Update
            | EntryMode::Query
            | EntryMode::InspectMessage => {
                let data = self.env.sender.as_slice();
                copy_to_canister(dst, offset, size, &data)?;
                Ok(())
            }
            _ => Err(format!(
                "msg_caller_copy can not be called from '{}'",
                self.env.get_entry_point_name()
            )),
        }
    }

    fn msg_reject_code(&mut self) -> Result<i32, String> {
        match self.env.entry_mode {
            EntryMode::CustomTask | EntryMode::ReplyCallback | EntryMode::RejectCallback => {
                Ok(self.env.rejection_code as i32)
            }
            _ => Err(format!(
                "msg_reject_code can not be called from '{}'",
                self.env.get_entry_point_name()
            )),
        }
    }

    fn msg_reject_msg_size(&mut self) -> Result<isize, String> {
        match self.env.entry_mode {
            EntryMode::CustomTask | EntryMode::RejectCallback => {
                Ok(self.env.rejection_message.len() as isize)
            }
            _ => Err(format!(
                "msg_reject_msg_size can not be called from '{}'",
                self.env.get_entry_point_name()
            )),
        }
    }

    fn msg_reject_msg_copy(
        &mut self,
        dst: isize,
        offset: isize,
        size: isize,
    ) -> Result<(), String> {
        match self.env.entry_mode {
            EntryMode::CustomTask | EntryMode::RejectCallback => {
                let data = self.env.rejection_message.as_bytes();
                copy_to_canister(dst, offset, size, data)?;
                Ok(())
            }
            _ => Err(format!(
                "msg_reject_msg_copy can not be called from '{}'",
                self.env.get_entry_point_name()
            )),
        }
    }

    fn msg_reply_data_append(&mut self, src: isize, size: isize) -> Result<(), String> {
        let message_id = match self.env.entry_mode {
            EntryMode::CustomTask
            | EntryMode::Update
            | EntryMode::Query
            | EntryMode::ReplyCallback
            | EntryMode::RejectCallback => {
                // this should always be present when processing a call.
                self.request_id
                    .expect("ic-kit: Unexpected canister state, request_id not set.")
            }
            _ => {
                return Err(format!(
                    "msg_reply_data_append can not be called from '{}'",
                    self.env.get_entry_point_name()
                ))
            }
        };

        if !self.msg_reply_senders.contains_key(&message_id) {
            return Err(format!(
                "msg_reply_data_append may only be invoked before canister responses."
            ));
        }

        let reply_data = self.msg_reply_data.entry(message_id).or_default();
        reply_data.extend_from_slice(copy_from_canister(src, size));

        Ok(())
    }

    fn msg_reply(&mut self) -> Result<(), String> {
        let message_id = match self.env.entry_mode {
            EntryMode::CustomTask
            | EntryMode::Update
            | EntryMode::Query
            | EntryMode::ReplyCallback
            | EntryMode::RejectCallback => {
                // this should always be present when processing a call.
                self.request_id
                    .expect("ic-kit: Unexpected canister state, request_id not set.")
            }
            _ => {
                return Err(format!(
                    "msg_reply can not be called from '{}'",
                    self.env.get_entry_point_name()
                ))
            }
        };

        let reply_chan = self
            .msg_reply_senders
            .remove(&message_id)
            .ok_or_else(|| format!("msg_reply may only be invoked before canister responses."))?;

        let data = self.msg_reply_data.remove(&message_id).unwrap_or_default();
        let cycles_refunded = self.env.cycles_available;
        reply_chan
            .send(CanisterReply::Reply {
                data,
                cycles_refunded,
            })
            .expect("ic-kit: could not send the response.");
        self.env.cycles_available = 0;

        Ok(())
    }

    fn msg_reject(&mut self, src: isize, size: isize) -> Result<(), String> {
        let message_id = match self.env.entry_mode {
            EntryMode::CustomTask
            | EntryMode::Update
            | EntryMode::Query
            | EntryMode::ReplyCallback
            | EntryMode::RejectCallback => {
                // this should always be present when processing a call.
                self.request_id
                    .expect("ic-kit: Unexpected canister state, request_id not set.")
            }
            _ => {
                return Err(format!(
                    "msg_reject can not be called from '{}'",
                    self.env.get_entry_point_name()
                ))
            }
        };

        let reply_chan = self
            .msg_reply_senders
            .remove(&message_id)
            .ok_or_else(|| format!("msg_reject may only be invoked before canister responses."))?;

        // we don't care about the data anymore.
        self.msg_reply_data.remove(&message_id);

        let cycles_refunded = self.env.cycles_available;
        let rejection_message = String::from_utf8_lossy(copy_from_canister(src, size)).into();

        reply_chan
            .send(CanisterReply::Reject {
                rejection_code: RejectionCode::CanisterReject,
                rejection_message,
                cycles_refunded,
            })
            .expect("ic-kit: could not send the response.");
        self.env.cycles_available = 0;

        Ok(())
    }

    fn msg_cycles_available(&mut self) -> Result<i64, String> {
        match self.env.entry_mode {
            EntryMode::CustomTask
            | EntryMode::Update
            | EntryMode::ReplyCallback
            | EntryMode::RejectCallback => {
                if self.env.cycles_available > (u64::MAX as u128) {
                    return Err(format!("available cycles does not fit in u64"));
                }

                Ok(self.env.cycles_available as u64 as i64)
            }
            _ => Err(format!(
                "msg_cycles_available can not be called from '{}'",
                self.env.get_entry_point_name()
            )),
        }
    }

    fn msg_cycles_available128(&mut self, dst: isize) -> Result<(), String> {
        match self.env.entry_mode {
            EntryMode::CustomTask
            | EntryMode::Update
            | EntryMode::ReplyCallback
            | EntryMode::RejectCallback => {
                let data = self.env.cycles_available.to_le_bytes();
                copy_to_canister(dst, 0, 16, &data)?;
                Ok(())
            }
            _ => Err(format!(
                "msg_cycles_available128 can not be called from '{}'",
                self.env.get_entry_point_name()
            )),
        }
    }

    fn msg_cycles_refunded(&mut self) -> Result<i64, String> {
        match self.env.entry_mode {
            EntryMode::CustomTask | EntryMode::ReplyCallback | EntryMode::RejectCallback => {
                if self.env.cycles_refunded > (u64::MAX as u128) {
                    return Err(format!("refunded cycles does not fit in u64"));
                }

                Ok(self.env.cycles_refunded as u64 as i64)
            }
            _ => Err(format!(
                "msg_cycles_refunded can not be called from '{}'",
                self.env.get_entry_point_name()
            )),
        }
    }

    fn msg_cycles_refunded128(&mut self, dst: isize) -> Result<(), String> {
        match self.env.entry_mode {
            EntryMode::CustomTask | EntryMode::ReplyCallback | EntryMode::RejectCallback => {
                let data = self.env.cycles_refunded.to_le_bytes();
                copy_to_canister(dst, 0, 16, &data)?;
                Ok(())
            }
            _ => Err(format!(
                "msg_cycles_refunded128 can not be called from '{}'",
                self.env.get_entry_point_name()
            )),
        }
    }

    fn msg_cycles_accept(&mut self, max_amount: i64) -> Result<i64, String> {
        let message_id = match self.env.entry_mode {
            EntryMode::CustomTask
            | EntryMode::Update
            | EntryMode::ReplyCallback
            | EntryMode::RejectCallback => self
                .request_id
                .expect("ic-kit: Unexpected canister state, request_id not set."),
            _ => {
                return Err(format!(
                    "msg_cycles_accept can not be called from '{}'",
                    self.env.get_entry_point_name()
                ))
            }
        };

        let amount = self.env.cycles_available.min(max_amount as u128);
        self.env.cycles_available -= amount;
        self.cycles_accepted += amount;
        self.cycles_available_store
            .insert(message_id, self.env.cycles_available);

        Ok(amount as i64)
    }

    fn msg_cycles_accept128(
        &mut self,
        max_amount_high: i64,
        max_amount_low: i64,
        dst: isize,
    ) -> Result<(), String> {
        let message_id = match self.env.entry_mode {
            EntryMode::CustomTask
            | EntryMode::Update
            | EntryMode::ReplyCallback
            | EntryMode::RejectCallback => self
                .request_id
                .expect("ic-kit: Unexpected canister state, request_id not set."),
            _ => {
                return Err(format!(
                    "msg_cycles_accept128 can not be called from '{}'",
                    self.env.get_entry_point_name()
                ))
            }
        };

        let max_amount =
            (max_amount_high as u128) + (max_amount_low as u128) * (u64::MAX as u128 + 1);
        let amount = self.env.cycles_available.min(max_amount);
        self.env.cycles_available -= amount;
        self.cycles_accepted += amount;
        self.cycles_available_store
            .insert(message_id, self.env.cycles_available);
        copy_to_canister(dst, 0, 16, &amount.to_le_bytes())?;

        Ok(())
    }

    fn canister_self_size(&mut self) -> Result<isize, String> {
        Ok(self.canister_id.len() as isize)
    }

    fn canister_self_copy(&mut self, dst: isize, offset: isize, size: isize) -> Result<(), String> {
        let data = self.canister_id.as_slice();
        copy_to_canister(dst, offset, size, data)?;
        Ok(())
    }

    fn canister_cycle_balance(&mut self) -> Result<i64, String> {
        todo!()
    }

    fn canister_cycle_balance128(&mut self, dst: isize) -> Result<(), String> {
        todo!()
    }

    fn canister_status(&mut self) -> Result<i32, String> {
        todo!()
    }

    fn msg_method_name_size(&mut self) -> Result<isize, String> {
        todo!()
    }

    fn msg_method_name_copy(
        &mut self,
        dst: isize,
        offset: isize,
        size: isize,
    ) -> Result<(), String> {
        todo!()
    }

    fn accept_message(&mut self) -> Result<(), String> {
        todo!()
    }

    fn call_new(
        &mut self,
        callee_src: isize,
        callee_size: isize,
        name_src: isize,
        name_size: isize,
        reply_fun: isize,
        reply_env: isize,
        reject_fun: isize,
        reject_env: isize,
    ) -> Result<(), String> {
        todo!()
    }

    fn call_on_cleanup(&mut self, fun: isize, env: isize) -> Result<(), String> {
        todo!()
    }

    fn call_data_append(&mut self, src: isize, size: isize) -> Result<(), String> {
        todo!()
    }

    fn call_cycles_add(&mut self, amount: i64) -> Result<(), String> {
        todo!()
    }

    fn call_cycles_add128(&mut self, amount_high: i64, amount_low: i64) -> Result<(), String> {
        todo!()
    }

    fn call_perform(&mut self) -> Result<i32, String> {
        todo!()
    }

    fn stable_size(&mut self) -> Result<i32, String> {
        todo!()
    }

    fn stable_grow(&mut self, new_pages: i32) -> Result<i32, String> {
        todo!()
    }

    fn stable_write(&mut self, offset: i32, src: isize, size: isize) -> Result<(), String> {
        todo!()
    }

    fn stable_read(&mut self, dst: isize, offset: i32, size: isize) -> Result<(), String> {
        todo!()
    }

    fn stable64_size(&mut self) -> Result<i64, String> {
        todo!()
    }

    fn stable64_grow(&mut self, new_pages: i64) -> Result<i64, String> {
        todo!()
    }

    fn stable64_write(&mut self, offset: i64, src: i64, size: i64) -> Result<(), String> {
        todo!()
    }

    fn stable64_read(&mut self, dst: i64, offset: i64, size: i64) -> Result<(), String> {
        todo!()
    }

    fn certified_data_set(&mut self, src: isize, size: isize) -> Result<(), String> {
        todo!()
    }

    fn data_certificate_present(&mut self) -> Result<i32, String> {
        todo!()
    }

    fn data_certificate_size(&mut self) -> Result<isize, String> {
        todo!()
    }

    fn data_certificate_copy(
        &mut self,
        dst: isize,
        offset: isize,
        size: isize,
    ) -> Result<(), String> {
        todo!()
    }

    fn time(&mut self) -> Result<i64, String> {
        todo!()
    }

    fn performance_counter(&mut self, counter_type: i32) -> Result<i64, String> {
        todo!()
    }

    fn debug_print(&mut self, src: isize, size: isize) -> Result<(), String> {
        todo!()
    }

    fn trap(&mut self, src: isize, size: isize) -> Result<(), String> {
        todo!()
    }
}

fn copy_to_canister(dst: isize, offset: isize, size: isize, data: &[u8]) -> Result<(), String> {
    let dst = dst as usize;
    let offset = offset as usize;
    let size = size as usize;

    if offset + size > data.len() {
        return Err("Out of bound read.".into());
    }

    let slice = unsafe { std::slice::from_raw_parts_mut(dst as *mut u8, size) };
    slice.copy_from_slice(&data[offset..offset + size]);
    Ok(())
}

fn copy_from_canister<'a>(src: isize, size: isize) -> &'a [u8] {
    let src = src as usize;
    let size = size as usize;

    unsafe { std::slice::from_raw_parts(src as *const u8, size) }
}

fn downcast_panic_payload(payload: &Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<&'static str>()
        .cloned()
        .map(String::from)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| String::from("Box<Any>"))
}
