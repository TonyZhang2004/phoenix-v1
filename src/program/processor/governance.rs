use std::mem::size_of;

use super::{
    cancel_multiple_orders::{process_cancel_orders, CancelUpToParams},
    withdraw::process_withdraw,
};
use crate::{
    program::{
        checkers::phoenix_checkers::MarketAccountInfo, error::assert_with_msg,
        load_with_dispatch_mut, status::MarketStatus, token_utils::close_market_vault,
        AuthorizedActionContext, ChangeMarketStatusContext, MarketHeader, PhoenixMarketContext,
        TombstoneMarketAndCloseVaultsContext,
    },
    quantities::QuoteLots,
    state::{markets::MarketEvent, Side},
};
use borsh::BorshDeserialize;
use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, program_error::ProgramError,
    pubkey::Pubkey, system_program,
};

/// This action can be taken by the market authority to remove the seat (on the Market account) of a
/// trader whose Seat account is no longer approved
///
/// It will also withdraw all funds to token accounts owned by the trader, but it will fail
/// if the trader has any open orders.
pub(crate) fn process_evict_seat<'a, 'info>(
    _program_id: &Pubkey,
    market_context: &PhoenixMarketContext<'a, 'info>,
    accounts: &'a [AccountInfo<'info>],
    _data: &[u8],
) -> ProgramResult {
    let AuthorizedActionContext {
        trader,
        vault_context,
        ..
    } = AuthorizedActionContext::load(market_context, accounts)?;

    process_withdraw(
        &market_context.market_info,
        trader.clone(),
        vault_context,
        None,
        None,
        true,
    )
}

/// This action can be taken by the market authority to cancel all orders of a
/// trader whose seat is no longer approved
pub(crate) fn process_force_cancel_orders<'a, 'info>(
    _program_id: &Pubkey,
    market_context: &PhoenixMarketContext<'a, 'info>,
    accounts: &'a [AccountInfo<'info>],
    data: &[u8],
    record_event_fn: &mut dyn FnMut(MarketEvent<Pubkey>),
) -> ProgramResult {
    let AuthorizedActionContext {
        trader,
        vault_context,
        ..
    } = AuthorizedActionContext::load(market_context, accounts)?;
    process_cancel_orders(
        &market_context.market_info,
        trader.key,
        Some(vault_context),
        CancelUpToParams::try_from_slice(data)?,
        record_event_fn,
    )
}

/// This function can only be called by the active successor of the current authority.
pub(crate) fn process_claim_authority<'a, 'info>(
    _program_id: &Pubkey,
    market_context: &PhoenixMarketContext<'a, 'info>,
    _data: &[u8],
) -> ProgramResult {
    let PhoenixMarketContext {
        market_info,
        signer: successor,
    } = market_context;
    market_info.assert_valid_successor(successor.key)?;
    market_info.get_header_mut()?.authority = *successor.key;
    Ok(())
}

/// The authority can be changed to a successor, but the successor must explicitly claim the
/// authority from the previous market authority
pub(crate) fn process_name_successor<'a, 'info>(
    _program_id: &Pubkey,
    market_context: &PhoenixMarketContext<'a, 'info>,
    data: &[u8],
) -> ProgramResult {
    let PhoenixMarketContext {
        market_info,
        signer: authority,
    } = market_context;
    market_info.assert_valid_authority(authority.key)?;
    let successor = Pubkey::try_from_slice(data)?;
    market_info.get_header_mut()?.successor = successor;
    Ok(())
}

/// This function can only be called by the current market authority to
/// modify the current market status (based on valid transitions)
pub(crate) fn process_change_market_status<'a, 'info>(
    _program_id: &Pubkey,
    market_context: &PhoenixMarketContext<'a, 'info>,
    accounts: &'a [AccountInfo<'info>],
    data: &[u8],
) -> ProgramResult {
    let ChangeMarketStatusContext {
        receiver: receiver_option,
    } = ChangeMarketStatusContext::load(accounts)?;
    let PhoenixMarketContext {
        market_info,
        signer: authority,
    } = market_context;
    market_info.assert_valid_authority(authority.key)?;
    let next_state = MarketStatus::try_from_slice(data)?;
    let status = market_info.get_header()?.status;
    // Ensure that the state transition is allowed
    MarketStatus::from(status).assert_valid_state_transition(&next_state)?;
    // Modify the state of the market
    match next_state {
        // When the market is tombstoned, its data is fully removed.
        MarketStatus::Tombstoned => {
            validate_tombstone_preconditions(market_info)?;
            // The market lamports are either transferred to the receiver or to the authority
            let receiver = match receiver_option {
                Some(r) => r,
                None => authority.as_ref(),
            };
            close_market_account(market_info, receiver)?;
        }
        // In all other cases, we simply update the status of the market
        _ => {
            market_info.get_header_mut()?.status = next_state as u64;
            phoenix_log!("Market status changed to {}", next_state);
        }
    }
    Ok(())
}

pub(crate) fn process_tombstone_market_and_close_vaults<'a, 'info>(
    _program_id: &Pubkey,
    market_context: &PhoenixMarketContext<'a, 'info>,
    accounts: &'a [AccountInfo<'info>],
    data: &[u8],
) -> ProgramResult {
    assert_with_msg(
        data.is_empty(),
        ProgramError::InvalidInstructionData,
        "TombstoneMarketAndCloseVaults does not accept instruction data",
    )?;

    let PhoenixMarketContext {
        market_info,
        signer: authority,
    } = market_context;
    market_info.assert_valid_authority(authority.key)?;
    let status = market_info.get_header()?.status;
    MarketStatus::from(status).assert_valid_state_transition(&MarketStatus::Tombstoned)?;
    validate_tombstone_preconditions(market_info)?;

    let ctx = TombstoneMarketAndCloseVaultsContext::load(market_context, accounts)?;
    close_market_vault(
        market_info.key,
        &ctx.base_vault.mint_key,
        ctx.base_vault.bump,
        ctx.token_program,
        &ctx.base_vault.account,
        ctx.rent_recipient,
    )?;
    close_market_vault(
        market_info.key,
        &ctx.quote_vault.mint_key,
        ctx.quote_vault.bump,
        ctx.token_program,
        &ctx.quote_vault.account,
        ctx.rent_recipient,
    )?;
    close_market_account(market_info, ctx.rent_recipient)
}

fn validate_tombstone_preconditions(market_info: &MarketAccountInfo<'_, '_>) -> ProgramResult {
    let mut market_data = market_info.try_borrow_mut_data()?;
    let market = load_with_dispatch_mut(
        &market_info.size_params,
        &mut market_data[size_of::<MarketHeader>()..],
    )?
    .inner;
    assert_with_msg(
        market.get_book(Side::Bid).is_empty() && market.get_book(Side::Ask).is_empty(),
        ProgramError::InvalidAccountData,
        &format!(
            "Invalid market status, must have no open orders, found {} bids and {} asks",
            market.get_book(Side::Bid).len(),
            market.get_book(Side::Ask).len()
        ),
    )?;
    assert_with_msg(
        market.get_uncollected_fee_amount() == QuoteLots::ZERO,
        ProgramError::InvalidAccountData,
        "Invalid market status, must have no uncollected fees",
    )?;
    assert_with_msg(
        market.get_registered_traders().is_empty(),
        ProgramError::InvalidAccountData,
        &format!(
            "Invalid market status, must have no traders, found {}",
            market.get_registered_traders().len()
        ),
    )
}

fn close_market_account(
    market_info: &MarketAccountInfo<'_, '_>,
    rent_recipient: &AccountInfo<'_>,
) -> ProgramResult {
    assert_with_msg(
        rent_recipient.is_writable && rent_recipient.key != market_info.key,
        ProgramError::InvalidInstructionData,
        "Invalid market rent recipient",
    )?;
    let recipient_lamports = rent_recipient
        .lamports()
        .checked_add(market_info.lamports())
        .ok_or(ProgramError::InvalidAccountData)?;
    **rent_recipient.lamports.borrow_mut() = recipient_lamports;
    **market_info.lamports.borrow_mut() = 0;
    market_info.assign(&system_program::id());
    market_info.realloc(0, false)?;
    phoenix_log!("Market has been removed");
    Ok(())
}
