//! Implement thread-local storage.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::collections::HashSet;

use log::trace;

use rustc_index::vec::Idx;
use rustc_middle::ty;
use rustc_target::abi::{Size, HasDataLayout};

use crate::{
    HelpersEvalContextExt, InterpResult, MPlaceTy, Scalar, StackPopCleanup, Tag, ThreadId,
    ThreadsEvalContextExt,
};

pub type TlsKey = u128;

#[derive(Clone, Debug)]
pub struct TlsEntry<'tcx> {
    /// The data for this key. None is used to represent NULL.
    /// (We normalize this early to avoid having to do a NULL-ptr-test each time we access the data.)
    /// Will eventually become a map from thread IDs to `Scalar`s, if we ever support more than one thread.
    data: BTreeMap<ThreadId, Scalar<Tag>>,
    dtor: Option<ty::Instance<'tcx>>,
}

#[derive(Debug)]
pub struct TlsData<'tcx> {
    /// The Key to use for the next thread-local allocation.
    next_key: TlsKey,

    /// pthreads-style thread-local storage.
    keys: BTreeMap<TlsKey, TlsEntry<'tcx>>,

    /// A single global per thread dtor (that's how things work on macOS) with a data argument.
    global_dtors: BTreeMap<ThreadId, (ty::Instance<'tcx>, Scalar<Tag>)>,

    /// Whether we are in the "destruct" phase, during which some operations are UB.
    dtors_running: HashSet<ThreadId>,
}

impl<'tcx> Default for TlsData<'tcx> {
    fn default() -> Self {
        TlsData {
            next_key: 1, // start with 1 as we must not use 0 on Windows
            keys: Default::default(),
            global_dtors: Default::default(),
            dtors_running: Default::default(),
        }
    }
}

impl<'tcx> TlsData<'tcx> {
    /// Generate a new TLS key with the given destructor.
    /// `max_size` determines the integer size the key has to fit in.
    pub fn create_tls_key(&mut self, dtor: Option<ty::Instance<'tcx>>, max_size: Size) -> InterpResult<'tcx, TlsKey> {
        let new_key = self.next_key;
        self.next_key += 1;
        self.keys.insert(new_key, TlsEntry { data: Default::default(), dtor }).unwrap_none();
        trace!("New TLS key allocated: {} with dtor {:?}", new_key, dtor);

        if max_size.bits() < 128 && new_key >= (1u128 << max_size.bits() as u128) {
            throw_unsup_format!("we ran out of TLS key space");
        }
        Ok(new_key)
    }

    pub fn delete_tls_key(&mut self, key: TlsKey) -> InterpResult<'tcx> {
        match self.keys.remove(&key) {
            Some(_) => {
                trace!("TLS key {} removed", key);
                Ok(())
            }
            None => throw_ub_format!("removing a non-existig TLS key: {}", key),
        }
    }

    pub fn load_tls(
        &self,
        key: TlsKey,
        thread_id: ThreadId,
        cx: &impl HasDataLayout,
    ) -> InterpResult<'tcx, Scalar<Tag>> {
        match self.keys.get(&key) {
            Some(TlsEntry { data, .. }) => {
                let value = data.get(&thread_id).cloned();
                trace!("TLS key {} for thread {:?} loaded: {:?}", key, thread_id, value);
                Ok(value.unwrap_or_else(|| Scalar::null_ptr(cx).into()))
            }
            None => throw_ub_format!("loading from a non-existing TLS key: {}", key),
        }
    }

    pub fn store_tls(
        &mut self,
         key: TlsKey, thread_id: ThreadId, new_data: Option<Scalar<Tag>>) -> InterpResult<'tcx> {
        match self.keys.get_mut(&key) {
            Some(TlsEntry { data, .. }) => {
                match new_data {
                    Some(ptr) => {
                        trace!("TLS key {} for thread {:?} stored: {:?}", key, thread_id, ptr);
                        data.insert(thread_id, ptr);
                    }
                    None => {
                        trace!("TLS key {} for thread {:?} removed", key, thread_id);
                        data.remove(&thread_id);
                    }
                }
                Ok(())
            }
            None => throw_ub_format!("storing to a non-existing TLS key: {}", key),
        }
    }

    /// Set global dtor for the given thread.
    pub fn set_global_dtor(&mut self, thread: ThreadId, dtor: ty::Instance<'tcx>, data: Scalar<Tag>) -> InterpResult<'tcx> {
        if self.dtors_running.contains(&thread) {
            // UB, according to libstd docs.
            throw_ub_format!("setting global destructor while destructors are already running");
        }
        if self.global_dtors.insert(thread, (dtor, data)).is_some() {
            throw_unsup_format!("setting more than one global destructor for the same thread is not supported");
        }
        Ok(())
    }

    /// Returns a dtor, its argument and its index, if one is supposed to run.
    /// `key` is the last dtors that was run; we return the *next* one after that.
    ///
    /// An optional destructor function may be associated with each key value.
    /// At thread exit, if a key value has a non-NULL destructor pointer,
    /// and the thread has a non-NULL value associated with that key,
    /// the value of the key is set to NULL, and then the function pointed
    /// to is called with the previously associated value as its sole argument.
    /// The order of destructor calls is unspecified if more than one destructor
    /// exists for a thread when it exits.
    ///
    /// If, after all the destructors have been called for all non-NULL values
    /// with associated destructors, there are still some non-NULL values with
    /// associated destructors, then the process is repeated.
    /// If, after at least {PTHREAD_DESTRUCTOR_ITERATIONS} iterations of destructor
    /// calls for outstanding non-NULL values, there are still some non-NULL values
    /// with associated destructors, implementations may stop calling destructors,
    /// or they may continue calling destructors until no non-NULL values with
    /// associated destructors exist, even though this might result in an infinite loop.
    fn fetch_tls_dtor(
        &mut self,
        key: Option<TlsKey>,
        thread_id: ThreadId,
    ) -> Option<(ty::Instance<'tcx>, Scalar<Tag>, TlsKey)> {
        use std::collections::Bound::*;

        let thread_local = &mut self.keys;
        let start = match key {
            Some(key) => Excluded(key),
            None => Unbounded,
        };
        for (&key, TlsEntry { data, dtor }) in
            thread_local.range_mut((start, Unbounded))
        {
            match data.entry(thread_id) {
                Entry::Occupied(entry) => {
                    let data_scalar = entry.remove();
                    if let Some(dtor) = dtor {
                        let ret = Some((*dtor, data_scalar, key));
                        return ret;
                    }
                }
                Entry::Vacant(_) => {}
            }
        }
        None
    }
}

impl<'mir, 'tcx: 'mir> EvalContextExt<'mir, 'tcx> for crate::MiriEvalContext<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: crate::MiriEvalContextExt<'mir, 'tcx> {

    /// Run TLS destructors for the main thread on Windows. The implementation
    /// assumes that we do not support concurrency on Windows yet.
    ///
    /// Note: on non-Windows OS this function is a no-op.
    fn run_windows_tls_dtors(&mut self) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        if this.tcx.sess.target.target.target_os != "windows" {
            return Ok(());
        }
        let active_thread = this.get_active_thread()?;
        assert_eq!(active_thread.index(), 0, "concurrency on Windows not supported");
        assert!(!this.machine.tls.dtors_running.contains(&active_thread), "running TLS dtors twice");
        this.machine.tls.dtors_running.insert(active_thread);
        // Windows has a special magic linker section that is run on certain events.
        // Instead of searching for that section and supporting arbitrary hooks in there
        // (that would be basically https://github.com/rust-lang/miri/issues/450),
        // we specifically look up the static in libstd that we know is placed
        // in that section.
        let thread_callback = this.eval_path_scalar(&["std", "sys", "windows", "thread_local", "p_thread_callback"])?;
        let thread_callback = this.memory.get_fn(thread_callback.not_undef()?)?.as_instance()?;

        // The signature of this function is `unsafe extern "system" fn(h: c::LPVOID, dwReason: c::DWORD, pv: c::LPVOID)`.
        let reason = this.eval_path_scalar(&["std", "sys", "windows", "c", "DLL_PROCESS_DETACH"])?;
        let ret_place = MPlaceTy::dangling(this.machine.layouts.unit, this).into();
        this.call_function(
            thread_callback,
            &[Scalar::null_ptr(this).into(), reason.into(), Scalar::null_ptr(this).into()],
            Some(ret_place),
            StackPopCleanup::None { cleanup: true },
        )?;

        // step until out of stackframes
        this.run()?;

        // Windows doesn't have other destructors.
        Ok(())
    }

    /// Run TLS destructors for the active thread.
    ///
    /// Note: on Windows OS this function is a no-op because we do not support
    /// concurrency on Windows yet.
    fn run_tls_dtors_for_active_thread(&mut self) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        if this.tcx.sess.target.target.target_os == "windows" {
            return Ok(());
        }
        let thread_id = this.get_active_thread()?;
        assert!(!this.machine.tls.dtors_running.contains(&thread_id), "running TLS dtors twice");
        this.machine.tls.dtors_running.insert(thread_id);

        // The macOS global dtor runs "before any TLS slots get freed", so do that first.
        if let Some(&(instance, data)) = this.machine.tls.global_dtors.get(&thread_id) {
            trace!("Running global dtor {:?} on {:?} at {:?}", instance, data, thread_id);

            let ret_place = MPlaceTy::dangling(this.machine.layouts.unit, this).into();
            this.call_function(
                instance,
                &[data.into()],
                Some(ret_place),
                StackPopCleanup::None { cleanup: true },
            )?;

            // step until out of stackframes
            this.run()?;
        }

        assert!(this.has_terminated(thread_id)?, "running TLS dtors for non-terminated thread");
        let mut dtor = this.machine.tls.fetch_tls_dtor(None, thread_id);
        while let Some((instance, ptr, key)) = dtor {
            trace!("Running TLS dtor {:?} on {:?} at {:?}", instance, ptr, thread_id);
            assert!(!this.is_null(ptr).unwrap(), "Data can't be NULL when dtor is called!");

            let ret_place = MPlaceTy::dangling(this.machine.layouts.unit, this).into();
            this.call_function(
                instance,
                &[ptr.into()],
                Some(ret_place),
                StackPopCleanup::None { cleanup: true },
            )?;

            // step until out of stackframes
            this.run()?;

            // Fetch next dtor after `key`.
            dtor = match this.machine.tls.fetch_tls_dtor(Some(key), thread_id) {
                dtor @ Some(_) => dtor,
                // We ran each dtor once, start over from the beginning.
                None => this.machine.tls.fetch_tls_dtor(None, thread_id),
            };
        }

        Ok(())
    }
}
