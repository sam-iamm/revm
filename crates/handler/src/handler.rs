use crate::{
    evm::FrameTr, execution, post_execution, pre_execution, validation, EvmTr, FrameResult,
    ItemOrResult,
};
use context::result::{ExecutionResult, FromStringError};
use context::LocalContextTr;
use context_interface::context::ContextError;
use context_interface::ContextTr;
use context_interface::{
    result::{HaltReasonTr, InvalidHeader, InvalidTransaction},
    Cfg, Database, JournalTr, Transaction,
};
use interpreter::interpreter_action::FrameInit;
use interpreter::{Gas, InitialAndFloorGas, SharedMemory};
use primitives::U256;

/// Trait for errors that can occur during EVM execution.
///
/// This trait represents the minimal error requirements for EVM execution,
/// ensuring that all necessary error types can be converted into the handler's error type.
pub trait EvmTrError<EVM: EvmTr>:
    From<InvalidTransaction>
    + From<InvalidHeader>
    + From<<<EVM::Context as ContextTr>::Db as Database>::Error>
    + From<ContextError<<<EVM::Context as ContextTr>::Db as Database>::Error>>
    + FromStringError
{
}

impl<
        EVM: EvmTr,
        T: From<InvalidTransaction>
            + From<InvalidHeader>
            + From<<<EVM::Context as ContextTr>::Db as Database>::Error>
            + From<ContextError<<<EVM::Context as ContextTr>::Db as Database>::Error>>
            + FromStringError,
    > EvmTrError<EVM> for T
{
}

/// The main implementation of Ethereum Mainnet transaction execution.
///
/// The [`Handler::run`] method serves as the entry point for execution and provides
/// out-of-the-box support for executing Ethereum mainnet transactions.
///
/// This trait allows EVM variants to customize execution logic by implementing
/// their own method implementations.
///
/// The handler logic consists of four phases:
///   * Validation - Validates tx/block/config fields and loads caller account and validates initial gas requirements and
///     balance checks.
///   * Pre-execution - Loads and warms accounts, deducts initial gas
///   * Execution - Executes the main frame loop, delegating to [`EvmTr`] for creating and running call frames.
///   * Post-execution - Calculates final refunds, validates gas floor, reimburses caller,
///     and rewards beneficiary
///
///
/// The [`Handler::catch_error`] method handles cleanup of intermediate state if an error
/// occurs during execution.
///
/// # Returns
///
/// Returns execution status, error, gas spend and logs. State change is not returned and it is
/// contained inside Context Journal. This setup allows multiple transactions to be chain executed.
///
/// To finalize the execution and obtain changed state, call [`JournalTr::finalize`] function.
pub trait Handler {
    /// The EVM type containing Context, Instruction, and Precompiles implementations.
    type Evm: EvmTr<
        Context: ContextTr<Journal: JournalTr, Local: LocalContextTr>,
        Frame: FrameTr<FrameInit = FrameInit, FrameResult = FrameResult>,
    >;
    /// The error type returned by this handler.
    type Error: EvmTrError<Self::Evm>;
    /// The halt reason type included in the output
    type HaltReason: HaltReasonTr;

    /// The main entry point for transaction execution.
    ///
    /// This method calls [`Handler::run_without_catch_error`] and if it returns an error,
    /// calls [`Handler::catch_error`] to handle the error and cleanup.
    ///
    /// The [`Handler::catch_error`] method ensures intermediate state is properly cleared.
    ///
    /// # Error handling
    ///
    /// In case of error, the journal can be in an inconsistent state and should be cleared by calling
    /// [`JournalTr::discard_tx`] method or dropped.
    ///
    /// # Returns
    ///
    /// Returns execution result, error, gas spend and logs.
    #[inline]
    fn run(
        &mut self,
        evm: &mut Self::Evm,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        // Run inner handler and catch all errors to handle cleanup.
        match self.run_without_catch_error(evm) {
            Ok(output) => Ok(output),
            Err(e) => self.catch_error(evm, e),
        }
    }

    /// Runs the system call.
    ///
    /// System call is a special transaction where caller is a [`crate::SYSTEM_ADDRESS`]
    ///
    /// It is used to call a system contracts and it skips all the `validation` and `pre-execution` and most of `post-execution` phases.
    /// For example it will not deduct the caller or reward the beneficiary.
    ///
    /// State changs can be obtained by calling [`JournalTr::finalize`] method from the [`EvmTr::Context`].
    ///
    /// # Error handling
    ///
    /// By design system call should not fail and should always succeed.
    /// In case of an error (If fetching account/storage on rpc fails), the journal can be in an inconsistent
    /// state and should be cleared by calling [`JournalTr::discard_tx`] method or dropped.
    #[inline]
    fn run_system_call(
        &mut self,
        evm: &mut Self::Evm,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        // dummy values that are not used.
        let init_and_floor_gas = InitialAndFloorGas::new(0, 0);
        // call execution and than output.
        match self
            .execution(evm, &init_and_floor_gas)
            .and_then(|exec_result| self.execution_result(evm, exec_result))
        {
            out @ Ok(_) => out,
            Err(e) => self.catch_error(evm, e),
        }
    }

    /// Called by [`Handler::run`] to execute the core handler logic.
    ///
    /// Executes the four phases in sequence: [Handler::validate],
    /// [Handler::pre_execution], [Handler::execution], [Handler::post_execution].
    ///
    /// Returns any errors without catching them or calling [`Handler::catch_error`].
    #[inline]
    fn run_without_catch_error(
        &mut self,
        evm: &mut Self::Evm,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        let init_and_floor_gas = self.validate(evm)?;
        let eip7702_refund = self.pre_execution(evm)? as i64;
        let mut exec_result = self.execution(evm, &init_and_floor_gas)?;
        self.post_execution(evm, &mut exec_result, init_and_floor_gas, eip7702_refund)?;

        // Prepare the output
        self.execution_result(evm, exec_result)
    }

    /// Validates the execution environment and transaction parameters.
    ///
    /// Calculates initial and floor gas requirements and verifies they are covered by the gas limit.
    ///
    /// Validation against state is done later in pre-execution phase in deduct_caller function.
    #[inline]
    fn validate(&self, evm: &mut Self::Evm) -> Result<InitialAndFloorGas, Self::Error> {
        self.validate_env(evm)?;
        self.validate_initial_tx_gas(evm)
    }

    /// Prepares the EVM state for execution.
    ///
    /// Loads the beneficiary account (EIP-3651: Warm COINBASE) and all accounts/storage from the access list (EIP-2929).
    ///
    /// Deducts the maximum possible fee from the caller's balance.
    ///
    /// For EIP-7702 transactions, applies the authorization list and delegates successful authorizations.
    /// Returns the gas refund amount from EIP-7702. Authorizations are applied before execution begins.
    #[inline]
    fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        self.validate_against_state_and_deduct_caller(evm)?;
        self.load_accounts(evm)?;

        let gas = self.apply_eip7702_auth_list(evm)?;
        Ok(gas)
    }

    /// Creates and executes the initial frame, then processes the execution loop.
    ///
    /// Always calls [Handler::last_frame_result] to handle returned gas from the call.
    #[inline]
    fn execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        let gas_limit = evm.ctx().tx().gas_limit() - init_and_floor_gas.initial_gas;
        // Create first frame action
        let first_frame_input = self.first_frame_input(evm, gas_limit)?;

        // Run execution loop
        let mut frame_result = self.run_exec_loop(evm, first_frame_input)?;

        // Handle last frame result
        self.last_frame_result(evm, &mut frame_result)?;
        Ok(frame_result)
    }

    /// Handles the final steps of transaction execution.
    ///
    /// Calculates final refunds and validates the gas floor (EIP-7623) to ensure minimum gas is spent.
    /// After EIP-7623, at least floor gas must be consumed.
    ///
    /// Reimburses unused gas to the caller and rewards the beneficiary with transaction fees.
    /// The effective gas price determines rewards, with the base fee being burned.
    ///
    /// Finally, finalizes output by returning the journal state and clearing internal state
    /// for the next execution.
    #[inline]
    fn post_execution(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
        init_and_floor_gas: InitialAndFloorGas,
        eip7702_gas_refund: i64,
    ) -> Result<(), Self::Error> {
        // Calculate final refund and add EIP-7702 refund to gas.
        self.refund(evm, exec_result, eip7702_gas_refund);
        // Ensure gas floor is met and minimum floor gas is spent.
        self.eip7623_check_gas_floor(evm, exec_result, init_and_floor_gas);
        // Return unused gas to caller
        self.reimburse_caller(evm, exec_result)?;
        // Pay transaction fees to beneficiary
        self.reward_beneficiary(evm, exec_result)?;
        Ok(())
    }

    /* VALIDATION */

    /// Validates block, transaction and configuration fields.
    ///
    /// Performs all validation checks that can be done without loading state.
    /// For example, verifies transaction gas limit is below block gas limit.
    #[inline]
    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        validation::validate_env(evm.ctx())
    }

    /// Calculates initial gas costs based on transaction type and input data.
    ///
    /// Includes additional costs for access list and authorization list.
    ///
    /// Verifies the initial cost does not exceed the transaction gas limit.
    #[inline]
    fn validate_initial_tx_gas(&self, evm: &Self::Evm) -> Result<InitialAndFloorGas, Self::Error> {
        let ctx = evm.ctx_ref();
        validation::validate_initial_tx_gas(ctx.tx(), ctx.cfg().spec().into()).map_err(From::from)
    }

    /* PRE EXECUTION */

    /// Loads access list and beneficiary account, marking them as warm in the [`context::Journal`].
    #[inline]
    fn load_accounts(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        pre_execution::load_accounts(evm)
    }

    /// Processes the authorization list, validating authority signatures, nonces and chain IDs.
    /// Applies valid authorizations to accounts.
    ///
    /// Returns the gas refund amount specified by EIP-7702.
    #[inline]
    fn apply_eip7702_auth_list(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        pre_execution::apply_eip7702_auth_list(evm.ctx())
    }

    /// Deducts maximum possible fee and transfer value from caller's balance.
    ///
    /// Unused fees are returned to caller after execution completes.
    #[inline]
    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<(), Self::Error> {
        pre_execution::validate_against_state_and_deduct_caller(evm.ctx())
    }

    /* EXECUTION */

    /// Creates initial frame input using transaction parameters, gas limit and configuration.
    #[inline]
    fn first_frame_input(
        &mut self,
        evm: &mut Self::Evm,
        gas_limit: u64,
    ) -> Result<FrameInit, Self::Error> {
        let memory =
            SharedMemory::new_with_buffer(evm.ctx().local().shared_memory_buffer().clone());
        let ctx = evm.ctx_ref();
        Ok(FrameInit {
            depth: 0,
            memory,
            frame_input: execution::create_init_frame(ctx.tx(), gas_limit),
        })
    }

    /// Processes the result of the initial call and handles returned gas.
    #[inline]
    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let instruction_result = frame_result.interpreter_result().result;
        let gas = frame_result.gas_mut();
        let remaining = gas.remaining();
        let refunded = gas.refunded();

        // Spend the gas limit. Gas is reimbursed when the tx returns successfully.
        *gas = Gas::new_spent(evm.ctx().tx().gas_limit());

        if instruction_result.is_ok_or_revert() {
            gas.erase_cost(remaining);
        }

        if instruction_result.is_ok() {
            gas.record_refund(refunded);
        }
        Ok(())
    }

    /* FRAMES */

    /// Executes the main frame processing loop.
    ///
    /// This loop manages the frame stack, processing each frame until execution completes.
    /// For each iteration:
    /// 1. Calls the current frame
    /// 2. Handles the returned frame input or result
    /// 3. Creates new frames or propagates results as needed
    #[inline]
    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame_input: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        let res = evm.frame_init(first_frame_input)?;

        if let ItemOrResult::Result(frame_result) = res {
            return Ok(frame_result);
        }

        loop {
            let call_or_result = evm.frame_run()?;

            let result = match call_or_result {
                ItemOrResult::Item(init) => {
                    match evm.frame_init(init)? {
                        ItemOrResult::Item(_) => {
                            continue;
                        }
                        // Do not pop the frame since no new frame was created
                        ItemOrResult::Result(result) => result,
                    }
                }
                ItemOrResult::Result(result) => result,
            };

            if let Some(result) = evm.frame_return_result(result)? {
                return Ok(result);
            }
        }
    }

    /* POST EXECUTION */

    /// Validates that the minimum gas floor requirements are satisfied.
    ///
    /// Ensures that at least the floor gas amount has been consumed during execution.
    #[inline]
    fn eip7623_check_gas_floor(
        &self,
        _evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        init_and_floor_gas: InitialAndFloorGas,
    ) {
        post_execution::eip7623_check_gas_floor(exec_result.gas_mut(), init_and_floor_gas)
    }

    /// Calculates the final gas refund amount, including any EIP-7702 refunds.
    #[inline]
    fn refund(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        eip7702_refund: i64,
    ) {
        let spec = evm.ctx().cfg().spec().into();
        post_execution::refund(spec, exec_result.gas_mut(), eip7702_refund)
    }

    /// Returns unused gas costs to the transaction sender's account.
    #[inline]
    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        post_execution::reimburse_caller(evm.ctx(), exec_result.gas_mut(), U256::ZERO)
            .map_err(From::from)
    }

    /// Transfers transaction fees to the block beneficiary's account.
    #[inline]
    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        post_execution::reward_beneficiary(evm.ctx(), exec_result.gas_mut()).map_err(From::from)
    }

    /// Processes the final execution output.
    ///
    /// This method, retrieves the final state from the journal, converts internal results to the external output format.
    /// Internal state is cleared and EVM is prepared for the next transaction.
    #[inline]
    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        match core::mem::replace(evm.ctx().error(), Ok(())) {
            Err(ContextError::Db(e)) => return Err(e.into()),
            Err(ContextError::Custom(e)) => return Err(Self::Error::from_string(e)),
            Ok(_) => (),
        }

        let exec_result = post_execution::output(evm.ctx(), result);

        // commit transaction
        evm.ctx().journal_mut().commit_tx();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        Ok(exec_result)
    }

    /// Handles cleanup when an error occurs during execution.
    ///
    /// Ensures the journal state is properly cleared before propagating the error.
    /// On happy path journal is cleared in [`Handler::execution_result`] method.
    #[inline]
    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        // clean up local context. Initcode cache needs to be discarded.
        evm.ctx().local_mut().clear();
        evm.ctx().journal_mut().discard_tx();
        evm.frame_stack().clear();
        Err(error)
    }
}
