//! Module containing the [`JournalInner`] that is part of [`crate::Journal`].
use crate::entry::SelfdestructionRevertStatus;

use super::JournalEntryTr;
use bytecode::Bytecode;
use context_interface::{
    context::{SStoreResult, SelfDestructResult, StateLoad},
    journaled_state::{AccountLoad, JournalCheckpoint, TransferError},
};
use core::mem;
use database_interface::Database;
use primitives::{
    hardfork::SpecId::{self, *},
    hash_map::Entry,
    Address, HashMap, HashSet, Log, StorageKey, StorageValue, B256, KECCAK_EMPTY, U256,
};
use state::{Account, EvmState, EvmStorageSlot, TransientStorage};
use std::vec::Vec;
/// Inner journal state that contains journal and state changes.
///
/// Spec Id is a essential information for the Journal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct JournalInner<ENTRY> {
    /// The current state
    pub state: EvmState,
    /// Transient storage that is discarded after every transaction.
    ///
    /// See [EIP-1153](https://eips.ethereum.org/EIPS/eip-1153).
    pub transient_storage: TransientStorage,
    /// Emitted logs
    pub logs: Vec<Log>,
    /// The current call stack depth
    pub depth: usize,
    /// The journal of state changes, one for each transaction
    pub journal: Vec<ENTRY>,
    /// Global transaction id that represent number of transactions executed (Including reverted ones).
    /// It can be different from number of `journal_history` as some transaction could be
    /// reverted or had a error on execution.
    ///
    /// This ID is used in `Self::state` to determine if account/storage is touched/warm/cold.
    pub transaction_id: usize,
    /// The spec ID for the EVM. Spec is required for some journal entries and needs to be set for
    /// JournalInner to be functional.
    ///
    /// If spec is set it assumed that precompile addresses are set as well for this particular spec.
    ///
    /// This spec is used for two things:
    ///
    /// - [EIP-161]: Prior to this EIP, Ethereum had separate definitions for empty and non-existing accounts.
    /// - [EIP-6780]: `SELFDESTRUCT` only in same transaction
    ///
    /// [EIP-161]: https://eips.ethereum.org/EIPS/eip-161
    /// [EIP-6780]: https://eips.ethereum.org/EIPS/eip-6780
    pub spec: SpecId,
    /// Warm loaded addresses are used to check if loaded address
    /// should be considered cold or warm loaded when the account
    /// is first accessed.
    ///
    /// Note that this not include newly loaded accounts, account and storage
    /// is considered warm if it is found in the `State`.
    pub warm_preloaded_addresses: HashSet<Address>,
    /// Warm coinbase address, stored separately to avoid cloning preloaded addresses.
    pub warm_coinbase_address: Option<Address>,
    /// Precompile addresses
    pub precompiles: HashSet<Address>,
}

impl<ENTRY: JournalEntryTr> Default for JournalInner<ENTRY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<ENTRY: JournalEntryTr> JournalInner<ENTRY> {
    /// Creates new [`JournalInner`].
    ///
    /// `warm_preloaded_addresses` is used to determine if address is considered warm loaded.
    /// In ordinary case this is precompile or beneficiary.
    pub fn new() -> JournalInner<ENTRY> {
        Self {
            state: HashMap::default(),
            transient_storage: TransientStorage::default(),
            logs: Vec::new(),
            journal: Vec::default(),
            transaction_id: 0,
            depth: 0,
            spec: SpecId::default(),
            warm_preloaded_addresses: HashSet::default(),
            precompiles: HashSet::default(),
            warm_coinbase_address: None,
        }
    }

    /// Returns the logs
    #[inline]
    pub fn take_logs(&mut self) -> Vec<Log> {
        mem::take(&mut self.logs)
    }

    /// Prepare for next transaction, by committing the current journal to history, incrementing the transaction id
    /// and returning the logs.
    ///
    /// This function is used to prepare for next transaction. It will save the current journal
    /// and clear the journal for the next transaction.
    ///
    /// `commit_tx` is used even for discarding transactions so transaction_id will be incremented.
    pub fn commit_tx(&mut self) {
        // Clears all field from JournalInner. Doing it this way to avoid
        // missing any field.
        let Self {
            state,
            transient_storage,
            logs,
            depth,
            journal,
            transaction_id,
            spec,
            warm_preloaded_addresses,
            precompiles,
            warm_coinbase_address,
        } = self;
        // Spec precompiles and state are not changed. It is always set again execution.
        let _ = spec;
        let _ = precompiles;
        let _ = state;
        transient_storage.clear();
        *depth = 0;

        // Do nothing with journal history so we can skip cloning present journal.
        journal.clear();

        // Clear coinbase address warming for next tx
        *warm_coinbase_address = None;
        // Load precompiles into warm_preloaded_addresses.
        // TODO for precompiles we can use max transaction_id so they are always touched warm loaded.
        // at least after state clear EIP.
        reset_preloaded_addresses(warm_preloaded_addresses, precompiles);
        // increment transaction id.
        *transaction_id += 1;
        logs.clear();
    }

    /// Discard the current transaction, by reverting the journal entries and incrementing the transaction id.
    pub fn discard_tx(&mut self) {
        // if there is no journal entries, there has not been any changes.
        let Self {
            state,
            transient_storage,
            logs,
            depth,
            journal,
            transaction_id,
            spec,
            warm_preloaded_addresses,
            warm_coinbase_address,
            precompiles,
        } = self;

        let is_spurious_dragon_enabled = spec.is_enabled_in(SPURIOUS_DRAGON);
        // iterate over all journals entries and revert our global state
        journal.drain(..).rev().for_each(|entry| {
            entry.revert(state, None, is_spurious_dragon_enabled);
        });
        transient_storage.clear();
        *depth = 0;
        logs.clear();
        *transaction_id += 1;
        // Clear coinbase address warming for next tx
        *warm_coinbase_address = None;
        reset_preloaded_addresses(warm_preloaded_addresses, precompiles);
    }

    /// Take the [`EvmState`] and clears the journal by resetting it to initial state.
    ///
    /// Note: Precompile addresses and spec are preserved and initial state of
    /// warm_preloaded_addresses will contain precompiles addresses.
    #[inline]
    pub fn finalize(&mut self) -> EvmState {
        // Clears all field from JournalInner. Doing it this way to avoid
        // missing any field.
        let Self {
            state,
            transient_storage,
            logs,
            depth,
            journal,
            transaction_id,
            spec,
            warm_preloaded_addresses,
            warm_coinbase_address,
            precompiles,
        } = self;
        // Spec is not changed. And it is always set again in execution.
        let _ = spec;
        // Clear coinbase address warming for next tx
        *warm_coinbase_address = None;
        // Load precompiles into warm_preloaded_addresses.
        reset_preloaded_addresses(warm_preloaded_addresses, precompiles);

        let state = mem::take(state);
        logs.clear();
        transient_storage.clear();

        // clear journal and journal history.
        journal.clear();
        *depth = 0;
        // reset transaction id.
        *transaction_id = 0;

        state
    }

    /// Return reference to state.
    #[inline]
    pub fn state(&mut self) -> &mut EvmState {
        &mut self.state
    }

    /// Sets SpecId.
    #[inline]
    pub fn set_spec_id(&mut self, spec: SpecId) {
        self.spec = spec;
    }

    /// Mark account as touched as only touched accounts will be added to state.
    /// This is especially important for state clear where touched empty accounts needs to
    /// be removed from state.
    #[inline]
    pub fn touch(&mut self, address: Address) {
        if let Some(account) = self.state.get_mut(&address) {
            Self::touch_account(&mut self.journal, address, account);
        }
    }

    /// Mark account as touched.
    #[inline]
    fn touch_account(journal: &mut Vec<ENTRY>, address: Address, account: &mut Account) {
        if !account.is_touched() {
            journal.push(ENTRY::account_touched(address));
            account.mark_touch();
        }
    }

    /// Returns the _loaded_ [Account] for the given address.
    ///
    /// This assumes that the account has already been loaded.
    ///
    /// # Panics
    ///
    /// Panics if the account has not been loaded and is missing from the state set.
    #[inline]
    pub fn account(&self, address: Address) -> &Account {
        self.state
            .get(&address)
            .expect("Account expected to be loaded") // Always assume that acc is already loaded
    }

    /// Set code and its hash to the account.
    ///
    /// Note: Assume account is warm and that hash is calculated from code.
    #[inline]
    pub fn set_code_with_hash(&mut self, address: Address, code: Bytecode, hash: B256) {
        let account = self.state.get_mut(&address).unwrap();
        Self::touch_account(&mut self.journal, address, account);

        self.journal.push(ENTRY::code_changed(address));

        account.info.code_hash = hash;
        account.info.code = Some(code);
    }

    /// Use it only if you know that acc is warm.
    ///
    /// Assume account is warm.
    ///
    /// In case of EIP-7702 code with zero address, the bytecode will be erased.
    #[inline]
    pub fn set_code(&mut self, address: Address, code: Bytecode) {
        if let Bytecode::Eip7702(eip7702_bytecode) = &code {
            if eip7702_bytecode.address().is_zero() {
                self.set_code_with_hash(address, Bytecode::default(), KECCAK_EMPTY);
                return;
            }
        }

        let hash = code.hash_slow();
        self.set_code_with_hash(address, code, hash)
    }

    /// Add journal entry for caller accounting.
    #[inline]
    pub fn caller_accounting_journal_entry(
        &mut self,
        address: Address,
        old_balance: U256,
        bump_nonce: bool,
    ) {
        // account balance changed.
        self.journal
            .push(ENTRY::balance_changed(address, old_balance));
        // account is touched.
        self.journal.push(ENTRY::account_touched(address));

        if bump_nonce {
            // nonce changed.
            self.journal.push(ENTRY::nonce_changed(address));
        }
    }

    /// Increments the balance of the account.
    ///
    /// Mark account as touched.
    #[inline]
    pub fn balance_incr<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
        balance: U256,
    ) -> Result<(), DB::Error> {
        let account = self.load_account(db, address)?.data;
        let old_balance = account.info.balance;
        account.info.balance = account.info.balance.saturating_add(balance);

        // march account as touched.
        if !account.is_touched() {
            account.mark_touch();
            self.journal.push(ENTRY::account_touched(address));
        }

        // add journal entry for balance increment.
        self.journal
            .push(ENTRY::balance_changed(address, old_balance));
        Ok(())
    }

    /// Increments the nonce of the account.
    #[inline]
    pub fn nonce_bump_journal_entry(&mut self, address: Address) {
        self.journal.push(ENTRY::nonce_changed(address));
    }

    /// Transfers balance from two accounts. Returns error if sender balance is not enough.
    #[inline]
    pub fn transfer<DB: Database>(
        &mut self,
        db: &mut DB,
        from: Address,
        to: Address,
        balance: U256,
    ) -> Result<Option<TransferError>, DB::Error> {
        if balance.is_zero() {
            self.load_account(db, to)?;
            let to_account = self.state.get_mut(&to).unwrap();
            Self::touch_account(&mut self.journal, to, to_account);
            return Ok(None);
        }
        // load accounts
        self.load_account(db, from)?;
        self.load_account(db, to)?;

        // sub balance from
        let from_account = self.state.get_mut(&from).unwrap();
        Self::touch_account(&mut self.journal, from, from_account);
        let from_balance = &mut from_account.info.balance;

        let Some(from_balance_decr) = from_balance.checked_sub(balance) else {
            return Ok(Some(TransferError::OutOfFunds));
        };
        *from_balance = from_balance_decr;

        // add balance to
        let to_account = &mut self.state.get_mut(&to).unwrap();
        Self::touch_account(&mut self.journal, to, to_account);
        let to_balance = &mut to_account.info.balance;
        let Some(to_balance_incr) = to_balance.checked_add(balance) else {
            return Ok(Some(TransferError::OverflowPayment));
        };
        *to_balance = to_balance_incr;
        // Overflow of U256 balance is not possible to happen on mainnet. We don't bother to return funds from from_acc.

        self.journal
            .push(ENTRY::balance_transfer(from, to, balance));

        Ok(None)
    }

    /// Creates account or returns false if collision is detected.
    ///
    /// There are few steps done:
    /// 1. Make created account warm loaded (AccessList) and this should
    ///    be done before subroutine checkpoint is created.
    /// 2. Check if there is collision of newly created account with existing one.
    /// 3. Mark created account as created.
    /// 4. Add fund to created account
    /// 5. Increment nonce of created account if SpuriousDragon is active
    /// 6. Decrease balance of caller account.
    ///
    /// # Panics
    ///
    /// Panics if the caller is not loaded inside the EVM state.
    /// This should have been done inside `create_inner`.
    #[inline]
    pub fn create_account_checkpoint(
        &mut self,
        caller: Address,
        target_address: Address,
        balance: U256,
        spec_id: SpecId,
    ) -> Result<JournalCheckpoint, TransferError> {
        // Enter subroutine
        let checkpoint = self.checkpoint();

        // Fetch balance of caller.
        let caller_balance = self.state.get(&caller).unwrap().info.balance;
        // Check if caller has enough balance to send to the created contract.
        if caller_balance < balance {
            self.checkpoint_revert(checkpoint);
            return Err(TransferError::OutOfFunds);
        }

        // Newly created account is present, as we just loaded it.
        let target_acc = self.state.get_mut(&target_address).unwrap();
        let last_journal = &mut self.journal;

        // New account can be created if:
        // Bytecode is not empty.
        // Nonce is not zero
        // Account is not precompile.
        if target_acc.info.code_hash != KECCAK_EMPTY || target_acc.info.nonce != 0 {
            self.checkpoint_revert(checkpoint);
            return Err(TransferError::CreateCollision);
        }

        // set account status to create.
        let is_created_globally = target_acc.mark_created_locally();

        // this entry will revert set nonce.
        last_journal.push(ENTRY::account_created(target_address, is_created_globally));
        target_acc.info.code = None;
        // EIP-161: State trie clearing (invariant-preserving alternative)
        if spec_id.is_enabled_in(SPURIOUS_DRAGON) {
            // nonce is going to be reset to zero in AccountCreated journal entry.
            target_acc.info.nonce = 1;
        }

        // touch account. This is important as for pre SpuriousDragon account could be
        // saved even empty.
        Self::touch_account(last_journal, target_address, target_acc);

        // Add balance to created account, as we already have target here.
        let Some(new_balance) = target_acc.info.balance.checked_add(balance) else {
            self.checkpoint_revert(checkpoint);
            return Err(TransferError::OverflowPayment);
        };
        target_acc.info.balance = new_balance;

        // safe to decrement for the caller as balance check is already done.
        self.state.get_mut(&caller).unwrap().info.balance -= balance;

        // add journal entry of transferred balance
        last_journal.push(ENTRY::balance_transfer(caller, target_address, balance));

        Ok(checkpoint)
    }

    /// Makes a checkpoint that in case of Revert can bring back state to this point.
    #[inline]
    pub fn checkpoint(&mut self) -> JournalCheckpoint {
        let checkpoint = JournalCheckpoint {
            log_i: self.logs.len(),
            journal_i: self.journal.len(),
        };
        self.depth += 1;
        checkpoint
    }

    /// Commits the checkpoint.
    #[inline]
    pub fn checkpoint_commit(&mut self) {
        self.depth -= 1;
    }

    /// Reverts all changes to state until given checkpoint.
    #[inline]
    pub fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        let is_spurious_dragon_enabled = self.spec.is_enabled_in(SPURIOUS_DRAGON);
        let state = &mut self.state;
        let transient_storage = &mut self.transient_storage;
        self.depth -= 1;
        self.logs.truncate(checkpoint.log_i);

        // iterate over last N journals sets and revert our global state
        self.journal
            .drain(checkpoint.journal_i..)
            .rev()
            .for_each(|entry| {
                entry.revert(state, Some(transient_storage), is_spurious_dragon_enabled);
            });
    }

    /// Performs selfdestruct action.
    /// Transfers balance from address to target. Check if target exist/is_cold
    ///
    /// Note: Balance will be lost if address and target are the same BUT when
    /// current spec enables Cancun, this happens only when the account associated to address
    /// is created in the same tx
    ///
    /// # References:
    ///  * <https://github.com/ethereum/go-ethereum/blob/141cd425310b503c5678e674a8c3872cf46b7086/core/vm/instructions.go#L832-L833>
    ///  * <https://github.com/ethereum/go-ethereum/blob/141cd425310b503c5678e674a8c3872cf46b7086/core/state/statedb.go#L449>
    ///  * <https://eips.ethereum.org/EIPS/eip-6780>
    #[inline]
    pub fn selfdestruct<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
        target: Address,
    ) -> Result<StateLoad<SelfDestructResult>, DB::Error> {
        let spec = self.spec;
        let account_load = self.load_account(db, target)?;
        let is_cold = account_load.is_cold;
        let is_empty = account_load.state_clear_aware_is_empty(spec);

        if address != target {
            // Both accounts are loaded before this point, `address` as we execute its contract.
            // and `target` at the beginning of the function.
            let acc_balance = self.state.get(&address).unwrap().info.balance;

            let target_account = self.state.get_mut(&target).unwrap();
            Self::touch_account(&mut self.journal, target, target_account);
            target_account.info.balance += acc_balance;
        }

        let acc = self.state.get_mut(&address).unwrap();
        let balance = acc.info.balance;

        let destroyed_status = if !acc.is_selfdestructed() {
            SelfdestructionRevertStatus::GloballySelfdestroyed
        } else if !acc.is_selfdestructed_locally() {
            SelfdestructionRevertStatus::LocallySelfdestroyed
        } else {
            SelfdestructionRevertStatus::RepeatedSelfdestruction
        };

        let is_cancun_enabled = spec.is_enabled_in(CANCUN);

        // EIP-6780 (Cancun hard-fork): selfdestruct only if contract is created in the same tx
        let journal_entry = if acc.is_created_locally() || !is_cancun_enabled {
            acc.mark_selfdestructed_locally();
            acc.info.balance = U256::ZERO;
            Some(ENTRY::account_destroyed(
                address,
                target,
                destroyed_status,
                balance,
            ))
        } else if address != target {
            acc.info.balance = U256::ZERO;
            Some(ENTRY::balance_transfer(address, target, balance))
        } else {
            // State is not changed:
            // * if we are after Cancun upgrade and
            // * Selfdestruct account that is created in the same transaction and
            // * Specify the target is same as selfdestructed account. The balance stays unchanged.
            None
        };

        if let Some(entry) = journal_entry {
            self.journal.push(entry);
        };

        Ok(StateLoad {
            data: SelfDestructResult {
                had_value: !balance.is_zero(),
                target_exists: !is_empty,
                previously_destroyed: destroyed_status
                    == SelfdestructionRevertStatus::RepeatedSelfdestruction,
            },
            is_cold,
        })
    }

    /// Loads account into memory. return if it is cold or warm accessed
    #[inline]
    pub fn load_account<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, DB::Error> {
        self.load_account_optional(db, address, false, [])
    }

    /// Loads account into memory. If account is EIP-7702 type it will additionally
    /// load delegated account.
    ///
    /// It will mark both this and delegated account as warm loaded.
    ///
    /// Returns information about the account (If it is empty or cold loaded) and if present the information
    /// about the delegated account (If it is cold loaded).
    #[inline]
    pub fn load_account_delegated<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
    ) -> Result<StateLoad<AccountLoad>, DB::Error> {
        let spec = self.spec;
        let is_eip7702_enabled = spec.is_enabled_in(SpecId::PRAGUE);
        let account = self.load_account_optional(db, address, is_eip7702_enabled, [])?;
        let is_empty = account.state_clear_aware_is_empty(spec);

        let mut account_load = StateLoad::new(
            AccountLoad {
                is_delegate_account_cold: None,
                is_empty,
            },
            account.is_cold,
        );

        // load delegate code if account is EIP-7702
        if let Some(Bytecode::Eip7702(code)) = &account.info.code {
            let address = code.address();
            let delegate_account = self.load_account(db, address)?;
            account_load.data.is_delegate_account_cold = Some(delegate_account.is_cold);
        }

        Ok(account_load)
    }

    /// Loads account and its code. If account is already loaded it will load its code.
    ///
    /// It will mark account as warm loaded. If not existing Database will be queried for data.
    ///
    /// In case of EIP-7702 delegated account will not be loaded,
    /// [`Self::load_account_delegated`] should be used instead.
    #[inline]
    pub fn load_code<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, DB::Error> {
        self.load_account_optional(db, address, true, [])
    }

    /// Loads account. If account is already loaded it will be marked as warm.
    #[inline]
    pub fn load_account_optional<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
        load_code: bool,
        storage_keys: impl IntoIterator<Item = StorageKey>,
    ) -> Result<StateLoad<&mut Account>, DB::Error> {
        let load = match self.state.entry(address) {
            Entry::Occupied(entry) => {
                let account = entry.into_mut();
                let is_cold = account.mark_warm_with_transaction_id(self.transaction_id);
                // if it is colad loaded we need to clear local flags that can interact with selfdestruct
                if is_cold {
                    // if it is cold loaded and we have selfdestructed locally it means that
                    // account was selfdestructed in previous transaction and we need to clear its information and storage.
                    if account.is_selfdestructed_locally() {
                        account.selfdestruct();
                        account.unmark_selfdestructed_locally();
                    }
                    // unmark locally created
                    account.unmark_created_locally();
                }
                StateLoad {
                    data: account,
                    is_cold,
                }
            }
            Entry::Vacant(vac) => {
                let account = if let Some(account) = db.basic(address)? {
                    account.into()
                } else {
                    Account::new_not_existing(self.transaction_id)
                };

                // Precompiles among some other account(coinbase included) are warm loaded so we need to take that into account
                let is_cold = !self.warm_preloaded_addresses.contains(&address)
                    && self.warm_coinbase_address.as_ref() != Some(&address);

                StateLoad {
                    data: vac.insert(account),
                    is_cold,
                }
            }
        };

        // journal loading of cold account.
        if load.is_cold {
            self.journal.push(ENTRY::account_warmed(address));
        }
        if load_code {
            let info = &mut load.data.info;
            if info.code.is_none() {
                let code = if info.code_hash == KECCAK_EMPTY {
                    Bytecode::default()
                } else {
                    db.code_by_hash(info.code_hash)?
                };
                info.code = Some(code);
            }
        }

        for storage_key in storage_keys.into_iter() {
            sload_with_account(
                load.data,
                db,
                &mut self.journal,
                self.transaction_id,
                address,
                storage_key,
            )?;
        }
        Ok(load)
    }

    /// Loads storage slot.
    ///
    /// # Panics
    ///
    /// Panics if the account is not present in the state.
    #[inline]
    pub fn sload<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
        key: StorageKey,
    ) -> Result<StateLoad<StorageValue>, DB::Error> {
        // assume acc is warm
        let account = self.state.get_mut(&address).unwrap();
        // only if account is created in this tx we can assume that storage is empty.
        sload_with_account(
            account,
            db,
            &mut self.journal,
            self.transaction_id,
            address,
            key,
        )
    }

    /// Stores storage slot.
    ///
    /// And returns (original,present,new) slot value.
    ///
    /// **Note**: Account should already be present in our state.
    #[inline]
    pub fn sstore<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
        key: StorageKey,
        new: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, DB::Error> {
        // assume that acc exists and load the slot.
        let present = self.sload(db, address, key)?;
        let acc = self.state.get_mut(&address).unwrap();

        // if there is no original value in dirty return present value, that is our original.
        let slot = acc.storage.get_mut(&key).unwrap();

        // new value is same as present, we don't need to do anything
        if present.data == new {
            return Ok(StateLoad::new(
                SStoreResult {
                    original_value: slot.original_value(),
                    present_value: present.data,
                    new_value: new,
                },
                present.is_cold,
            ));
        }

        self.journal
            .push(ENTRY::storage_changed(address, key, present.data));
        // insert value into present state.
        slot.present_value = new;
        Ok(StateLoad::new(
            SStoreResult {
                original_value: slot.original_value(),
                present_value: present.data,
                new_value: new,
            },
            present.is_cold,
        ))
    }

    /// Read transient storage tied to the account.
    ///
    /// EIP-1153: Transient storage opcodes
    #[inline]
    pub fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue {
        self.transient_storage
            .get(&(address, key))
            .copied()
            .unwrap_or_default()
    }

    /// Store transient storage tied to the account.
    ///
    /// If values is different add entry to the journal
    /// so that old state can be reverted if that action is needed.
    ///
    /// EIP-1153: Transient storage opcodes
    #[inline]
    pub fn tstore(&mut self, address: Address, key: StorageKey, new: StorageValue) {
        let had_value = if new.is_zero() {
            // if new values is zero, remove entry from transient storage.
            // if previous values was some insert it inside journal.
            // If it is none nothing should be inserted.
            self.transient_storage.remove(&(address, key))
        } else {
            // insert values
            let previous_value = self
                .transient_storage
                .insert((address, key), new)
                .unwrap_or_default();

            // check if previous value is same
            if previous_value != new {
                // if it is different, insert previous values inside journal.
                Some(previous_value)
            } else {
                None
            }
        };

        if let Some(had_value) = had_value {
            // insert in journal only if value was changed.
            self.journal
                .push(ENTRY::transient_storage_changed(address, key, had_value));
        }
    }

    /// Pushes log into subroutine.
    #[inline]
    pub fn log(&mut self, log: Log) {
        self.logs.push(log);
    }
}

/// Loads storage slot with account.
#[inline]
pub fn sload_with_account<DB: Database, ENTRY: JournalEntryTr>(
    account: &mut Account,
    db: &mut DB,
    journal: &mut Vec<ENTRY>,
    transaction_id: usize,
    address: Address,
    key: StorageKey,
) -> Result<StateLoad<StorageValue>, DB::Error> {
    let is_newly_created = account.is_created();
    let (value, is_cold) = match account.storage.entry(key) {
        Entry::Occupied(occ) => {
            let slot = occ.into_mut();
            let is_cold = slot.mark_warm_with_transaction_id(transaction_id);
            (slot.present_value, is_cold)
        }
        Entry::Vacant(vac) => {
            // if storage was cleared, we don't need to ping db.
            let value = if is_newly_created {
                StorageValue::ZERO
            } else {
                db.storage(address, key)?
            };

            vac.insert(EvmStorageSlot::new(value, transaction_id));

            (value, true)
        }
    };

    if is_cold {
        // add it to journal as cold loaded.
        journal.push(ENTRY::storage_warmed(address, key));
    }

    Ok(StateLoad::new(value, is_cold))
}

fn reset_preloaded_addresses(
    warm_preloaded_addresses: &mut HashSet<Address>,
    precompiles: &HashSet<Address>,
) {
    // `warm_preloaded_addresses` is append-only, and is initialized with `precompiles`.
    // Avoid unnecessarily cloning if it hasn't changed.
    if warm_preloaded_addresses.len() == precompiles.len() {
        debug_assert_eq!(warm_preloaded_addresses, precompiles);
        return;
    }
    warm_preloaded_addresses.clone_from(precompiles);
}
