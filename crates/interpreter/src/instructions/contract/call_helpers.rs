use crate::{
    gas,
    interpreter::Interpreter,
    interpreter_types::{InterpreterTypes, MemoryTr, RuntimeFlag, StackTr},
};
use context_interface::{context::StateLoad, journaled_state::AccountLoad};
use core::{cmp::min, ops::Range};
use primitives::{hardfork::SpecId::*, U256};

/// Gets memory input and output ranges for call instructions.
#[inline]
pub fn get_memory_input_and_out_ranges(
    interpreter: &mut Interpreter<impl InterpreterTypes>,
) -> Option<(Range<usize>, Range<usize>)> {
    popn!([in_offset, in_len, out_offset, out_len], interpreter, None);

    let mut in_range = resize_memory(interpreter, in_offset, in_len)?;

    if !in_range.is_empty() {
        let offset = interpreter.memory.local_memory_offset();
        in_range = in_range.start.saturating_add(offset)..in_range.end.saturating_add(offset);
    }

    let ret_range = resize_memory(interpreter, out_offset, out_len)?;
    Some((in_range, ret_range))
}

/// Resize memory and return range of memory.
/// If `len` is 0 dont touch memory and return `usize::MAX` as offset and 0 as length.
#[inline]
pub fn resize_memory(
    interpreter: &mut Interpreter<impl InterpreterTypes>,
    offset: U256,
    len: U256,
) -> Option<Range<usize>> {
    let len = as_usize_or_fail_ret!(interpreter, len, None);
    let offset = if len != 0 {
        let offset = as_usize_or_fail_ret!(interpreter, offset, None);
        resize_memory!(interpreter, offset, len, None);
        offset
    } else {
        usize::MAX //unrealistic value so we are sure it is not used
    };
    Some(offset..offset + len)
}

/// Calculates gas cost and limit for call instructions.
#[inline]
pub fn calc_call_gas(
    interpreter: &mut Interpreter<impl InterpreterTypes>,
    account_load: StateLoad<AccountLoad>,
    has_transfer: bool,
    local_gas_limit: u64,
) -> Option<u64> {
    let call_cost = gas::call_cost(
        interpreter.runtime_flag.spec_id(),
        has_transfer,
        account_load,
    );
    gas!(interpreter, call_cost, None);

    // EIP-150: Gas cost changes for IO-heavy operations
    let gas_limit = if interpreter.runtime_flag.spec_id().is_enabled_in(TANGERINE) {
        // Take l64 part of gas_limit
        min(interpreter.gas.remaining_63_of_64_parts(), local_gas_limit)
    } else {
        local_gas_limit
    };

    Some(gas_limit)
}
