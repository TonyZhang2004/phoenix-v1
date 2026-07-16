use super::token_checkers::TokenAccountInfo;
use crate::program::{accounts::TokenParams, error::assert_with_msg};
use solana_program::{
    account_info::AccountInfo, program_error::ProgramError, program_pack::Pack, pubkey::Pubkey,
};

pub fn get_vault_address(market: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"vault", market.as_ref(), mint.as_ref()], &crate::ID)
}

/// A canonical, empty market vault for the close-vault path.
///
/// SPL Token validates the effective close authority when the close CPI executes.
#[derive(Clone)]
pub(crate) struct MarketVault<'a, 'info> {
    pub(crate) account: TokenAccountInfo<'a, 'info>,
    pub(crate) mint_key: Pubkey,
    pub(crate) bump: u8,
}

impl<'a, 'info> MarketVault<'a, 'info> {
    pub(crate) fn new(
        market: &Pubkey,
        vault_info: &'a AccountInfo<'info>,
        params: &TokenParams,
    ) -> Result<Self, ProgramError> {
        let (expected_vault, expected_bump) = get_vault_address(market, &params.mint_key);
        assert_with_msg(
            expected_vault == params.vault_key && u32::from(expected_bump) == params.vault_bump,
            ProgramError::InvalidAccountData,
            "Market header contains invalid vault derivation data",
        )?;

        let account = TokenAccountInfo::new_with_owner_and_key(
            vault_info,
            &params.mint_key,
            &expected_vault,
            &expected_vault,
        )?;
        let token_account = spl_token::state::Account::unpack(&vault_info.try_borrow_data()?)?;
        assert_with_msg(
            token_account.amount == 0,
            ProgramError::InvalidAccountData,
            "Market vault must be empty before it can be closed",
        )?;

        Ok(Self {
            account,
            mint_key: params.mint_key,
            bump: expected_bump,
        })
    }
}
