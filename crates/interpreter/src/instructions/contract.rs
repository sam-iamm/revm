mod call_helpers;

pub use call_helpers::{calc_call_gas, get_memory_input_and_out_ranges, resize_memory};

use crate::{
    gas,
    instructions::utility::IntoAddress,
    interpreter_action::FrameInput,
    interpreter_types::{InputsTr, InterpreterTypes, LoopControl, MemoryTr, RuntimeFlag, StackTr},
    CallInput, CallInputs, CallScheme, CallValue, CreateInputs, Host, InstructionResult,
    InterpreterAction,
};
use context_interface::CreateScheme;
use primitives::{hardfork::SpecId, Address, Bytes, B256, U256};
use std::boxed::Box;

use crate::InstructionContext;

/// Implements the CREATE/CREATE2 instruction.
///
/// Creates a new contract with provided bytecode.
pub fn create<WIRE: InterpreterTypes, const IS_CREATE2: bool, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    require_non_staticcall!(context.interpreter);

    // EIP-1014: Skinny CREATE2
    if IS_CREATE2 {
        check!(context.interpreter, PETERSBURG);
    }

    popn!([value, code_offset, len], context.interpreter);
    let len = as_usize_or_fail!(context.interpreter, len);

    let mut code = Bytes::new();
    if len != 0 {
        // EIP-3860: Limit and meter initcode
        if context
            .interpreter
            .runtime_flag
            .spec_id()
            .is_enabled_in(SpecId::SHANGHAI)
        {
            // Limit is set as double of max contract bytecode size
            if len > context.host.max_initcode_size() {
                context
                    .interpreter
                    .halt(InstructionResult::CreateInitCodeSizeLimit);
                return;
            }
            gas!(context.interpreter, gas::initcode_cost(len));
        }

        let code_offset = as_usize_or_fail!(context.interpreter, code_offset);
        resize_memory!(context.interpreter, code_offset, len);
        code = Bytes::copy_from_slice(
            context
                .interpreter
                .memory
                .slice_len(code_offset, len)
                .as_ref(),
        );
    }

    // EIP-1014: Skinny CREATE2
    let scheme = if IS_CREATE2 {
        popn!([salt], context.interpreter);
        // SAFETY: `len` is reasonable in size as gas for it is already deducted.
        gas_or_fail!(context.interpreter, gas::create2_cost(len));
        CreateScheme::Create2 { salt }
    } else {
        gas!(context.interpreter, gas::CREATE);
        CreateScheme::Create
    };

    let mut gas_limit = context.interpreter.gas.remaining();

    // EIP-150: Gas cost changes for IO-heavy operations
    if context
        .interpreter
        .runtime_flag
        .spec_id()
        .is_enabled_in(SpecId::TANGERINE)
    {
        // Take remaining gas and deduce l64 part of it.
        gas_limit -= gas_limit / 64
    }
    gas!(context.interpreter, gas_limit);

    // Call host to interact with target contract
    context
        .interpreter
        .bytecode
        .set_action(InterpreterAction::NewFrame(FrameInput::Create(Box::new(
            CreateInputs {
                caller: context.interpreter.input.target_address(),
                scheme,
                value,
                init_code: code,
                gas_limit,
            },
        ))));
}

/// Implements the CALL instruction.
///
/// Message call with value transfer to another account.
pub fn call<WIRE: InterpreterTypes, H: Host + ?Sized>(context: InstructionContext<'_, H, WIRE>) {
    popn!([local_gas_limit, to, value], context.interpreter);
    let to = to.into_address();
    // Max gas limit is not possible in real ethereum situation.
    let local_gas_limit = u64::try_from(local_gas_limit).unwrap_or(u64::MAX);

    let has_transfer = !value.is_zero();
    if context.interpreter.runtime_flag.is_static() && has_transfer {
        context
            .interpreter
            .halt(InstructionResult::CallNotAllowedInsideStatic);
        return;
    }

    let Some((input, return_memory_offset)) = get_memory_input_and_out_ranges(context.interpreter)
    else {
        return;
    };

    let Some(account_load) = context.host.load_account_delegated(to) else {
        context
            .interpreter
            .halt(InstructionResult::FatalExternalError);
        return;
    };

    let Some(mut gas_limit) = calc_call_gas(
        context.interpreter,
        account_load,
        has_transfer,
        local_gas_limit,
    ) else {
        return;
    };

    gas!(context.interpreter, gas_limit);

    // Add call stipend if there is value to be transferred.
    if has_transfer {
        gas_limit = gas_limit.saturating_add(gas::CALL_STIPEND);
    }

    // Call host to interact with target contract
    context
        .interpreter
        .bytecode
        .set_action(InterpreterAction::NewFrame(FrameInput::Call(Box::new(
            CallInputs {
                input: CallInput::SharedBuffer(input),
                gas_limit,
                target_address: to,
                caller: context.interpreter.input.target_address(),
                bytecode_address: to,
                value: CallValue::Transfer(value),
                scheme: CallScheme::Call,
                is_static: context.interpreter.runtime_flag.is_static(),
                return_memory_offset,
            },
        ))));
}

/// Implements the CALLCODE instruction.
///
/// Message call with alternative account's code.
pub fn call_code<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    popn!([local_gas_limit, to, value], context.interpreter);
    let to = Address::from_word(B256::from(to));
    // Max gas limit is not possible in real ethereum situation.
    let local_gas_limit = u64::try_from(local_gas_limit).unwrap_or(u64::MAX);

    //pop!(context.interpreter, value);
    let Some((input, return_memory_offset)) = get_memory_input_and_out_ranges(context.interpreter)
    else {
        return;
    };

    let Some(mut load) = context.host.load_account_delegated(to) else {
        context
            .interpreter
            .halt(InstructionResult::FatalExternalError);
        return;
    };

    // Set `is_empty` to false as we are not creating this account.
    load.is_empty = false;
    let Some(mut gas_limit) =
        calc_call_gas(context.interpreter, load, !value.is_zero(), local_gas_limit)
    else {
        return;
    };

    gas!(context.interpreter, gas_limit);

    // Add call stipend if there is value to be transferred.
    if !value.is_zero() {
        gas_limit = gas_limit.saturating_add(gas::CALL_STIPEND);
    }

    // Call host to interact with target contract
    context
        .interpreter
        .bytecode
        .set_action(InterpreterAction::NewFrame(FrameInput::Call(Box::new(
            CallInputs {
                input: CallInput::SharedBuffer(input),
                gas_limit,
                target_address: context.interpreter.input.target_address(),
                caller: context.interpreter.input.target_address(),
                bytecode_address: to,
                value: CallValue::Transfer(value),
                scheme: CallScheme::CallCode,
                is_static: context.interpreter.runtime_flag.is_static(),
                return_memory_offset,
            },
        ))));
}

/// Implements the DELEGATECALL instruction.
///
/// Message call with alternative account's code but same sender and value.
pub fn delegate_call<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    check!(context.interpreter, HOMESTEAD);
    popn!([local_gas_limit, to], context.interpreter);
    let to = Address::from_word(B256::from(to));
    // Max gas limit is not possible in real ethereum situation.
    let local_gas_limit = u64::try_from(local_gas_limit).unwrap_or(u64::MAX);

    let Some((input, return_memory_offset)) = get_memory_input_and_out_ranges(context.interpreter)
    else {
        return;
    };

    let Some(mut load) = context.host.load_account_delegated(to) else {
        context
            .interpreter
            .halt(InstructionResult::FatalExternalError);
        return;
    };

    // Set is_empty to false as we are not creating this account.
    load.is_empty = false;
    let Some(gas_limit) = calc_call_gas(context.interpreter, load, false, local_gas_limit) else {
        return;
    };

    gas!(context.interpreter, gas_limit);

    // Call host to interact with target contract
    context
        .interpreter
        .bytecode
        .set_action(InterpreterAction::NewFrame(FrameInput::Call(Box::new(
            CallInputs {
                input: CallInput::SharedBuffer(input),
                gas_limit,
                target_address: context.interpreter.input.target_address(),
                caller: context.interpreter.input.caller_address(),
                bytecode_address: to,
                value: CallValue::Apparent(context.interpreter.input.call_value()),
                scheme: CallScheme::DelegateCall,
                is_static: context.interpreter.runtime_flag.is_static(),
                return_memory_offset,
            },
        ))));
}

/// Implements the STATICCALL instruction.
///
/// Static message call (cannot modify state).
pub fn static_call<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    check!(context.interpreter, BYZANTIUM);
    popn!([local_gas_limit, to], context.interpreter);
    let to = Address::from_word(B256::from(to));
    // Max gas limit is not possible in real ethereum situation.
    let local_gas_limit = u64::try_from(local_gas_limit).unwrap_or(u64::MAX);

    let Some((input, return_memory_offset)) = get_memory_input_and_out_ranges(context.interpreter)
    else {
        return;
    };

    let Some(mut load) = context.host.load_account_delegated(to) else {
        context
            .interpreter
            .halt(InstructionResult::FatalExternalError);
        return;
    };
    // Set `is_empty` to false as we are not creating this account.
    load.is_empty = false;
    let Some(gas_limit) = calc_call_gas(context.interpreter, load, false, local_gas_limit) else {
        return;
    };
    gas!(context.interpreter, gas_limit);

    // Call host to interact with target contract
    context
        .interpreter
        .bytecode
        .set_action(InterpreterAction::NewFrame(FrameInput::Call(Box::new(
            CallInputs {
                input: CallInput::SharedBuffer(input),
                gas_limit,
                target_address: to,
                caller: context.interpreter.input.target_address(),
                bytecode_address: to,
                value: CallValue::Transfer(U256::ZERO),
                scheme: CallScheme::StaticCall,
                is_static: true,
                return_memory_offset,
            },
        ))));
}
