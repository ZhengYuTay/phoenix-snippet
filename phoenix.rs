use anchor_lang::{declare_id, ToAccountMetas};
use anyhow::{anyhow, ensure, Context, Result};
use jupiter::{accounts::PhoenixSwap, Side};
use phoenix::{
    program::{load_with_dispatch, MarketHeader},
    quantities::WrapperU64,
    state::markets::{Ladder, LadderOrder},
};
use rust_decimal::{prelude::FromPrimitive, Decimal};
use rust_decimal_macros::dec;
use std::{collections::HashMap, mem::size_of};

use crate::{
    amm::{try_get_account_data, AccountMap},
    amms::amm::{Amm, KeyedAccount, Quote, QuoteParams, SwapAndAccountMetas, SwapParams},
};
use solana_sdk::{pubkey::Pubkey, sysvar};

use jupiter::jupiter_override::Swap;

declare_id!("PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY");

const BPS_TO_PCT: Decimal = dec!(10_000);

#[derive(Clone, Debug)]
pub struct PhoenixAmm {
    /// The pubkey of the market account
    market_key: Pubkey,
    /// The pubkey of the base mint
    base_mint: Pubkey,
    /// The pubkey of the quote mint
    quote_mint: Pubkey,
    /// Only here for convenience
    base_decimals: u32,
    /// Only here for convenience
    quote_decimals: u32,
    /// The size of a base lot in base atoms
    base_lot_size: u64,
    /// The size of a quote lot in quote atoms
    quote_lot_size: u64,
    /// The number of a base lot in a base unit
    base_lots_per_base_unit: u64,
    /// The number of a quote lots per base unit in a tick (tick_size)
    tick_size_in_quote_lots_per_base_unit_per_tick: u64,
    /// Taker fee basis points
    taker_fee_bps: u64,
    /// Fee pct
    fee_pct: Decimal,
    /// The state of the orderbook (L2)
    ladder: Option<Ladder>,
}

impl PhoenixAmm {
    pub fn from_keyed_account(keyed_account: &KeyedAccount) -> Result<Self> {
        let (header_bytes, bytes) = keyed_account
            .account
            .data
            .split_at(size_of::<MarketHeader>());
        let header: &MarketHeader = bytemuck::try_from_bytes(header_bytes)
            .map_err(|e| anyhow!("Error getting market header. Error: {:?}", e))?;
        let market = load_with_dispatch(&header.market_size_params, bytes)
            .map_err(|e| anyhow!("Failed to load market. Error {:?}", e))?;
        let taker_fee_bps = market.inner.get_taker_fee_bps();
        let fee_pct =
            PhoenixAmm::compute_fee_pct(taker_fee_bps).context("Cannot compute fee pct")?;
        Ok(Self {
            market_key: keyed_account.key,
            base_mint: header.base_params.mint_key,
            quote_mint: header.quote_params.mint_key,
            base_decimals: header.base_params.decimals,
            quote_decimals: header.quote_params.decimals,
            taker_fee_bps,
            fee_pct,
            base_lot_size: header.get_base_lot_size().as_u64(),
            quote_lot_size: header.get_quote_lot_size().as_u64(),
            base_lots_per_base_unit: market.inner.get_base_lots_per_base_unit().as_u64(),
            tick_size_in_quote_lots_per_base_unit_per_tick: header
                .get_tick_size_in_quote_atoms_per_base_unit()
                .as_u64()
                / header.get_quote_lot_size().as_u64(),
            ladder: None,
        })
    }

    fn compute_fee_pct(taker_fee_bps: u64) -> Option<Decimal> {
        Decimal::from_u64(taker_fee_bps)?.checked_div(BPS_TO_PCT)
    }

    fn compute_decimal_div(a: u64, b: u64) -> Option<Decimal> {
        Decimal::from_u64(a)?.checked_div(Decimal::from_u64(b)?)
    }

    fn compute_price_impact_pct(price: Decimal, best_price: Decimal) -> Option<Decimal> {
        best_price.checked_sub(price)?.checked_div(best_price)
    }

    pub fn get_base_decimals(&self) -> u32 {
        self.base_decimals
    }

    pub fn get_quote_decimals(&self) -> u32 {
        self.quote_decimals
    }
}

impl Amm for PhoenixAmm {
    fn label(&self) -> String {
        "Phoenix".into()
    }

    fn program_id(&self) -> Pubkey {
        self::id()
    }

    fn key(&self) -> Pubkey {
        self.market_key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![self.base_mint, self.quote_mint]
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        vec![self.market_key, sysvar::clock::ID]
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        let market_account_data = try_get_account_data(account_map, &self.market_key)?;
        let sysvar_clock_data = try_get_account_data(account_map, &sysvar::clock::ID)?;
        let clock: sysvar::clock::Clock = bincode::deserialize(sysvar_clock_data)?;

        let (header_bytes, bytes) = market_account_data.split_at(size_of::<MarketHeader>());
        let header: &MarketHeader = bytemuck::try_from_bytes(header_bytes)
            .map_err(|e| anyhow!("Error getting market header. Error: {:?}", e))?;
        let market = load_with_dispatch(&header.market_size_params, bytes)
            .map_err(|e| anyhow!("Failed to load market. Error {:?}", e))?;
        self.ladder = Some(market.inner.get_ladder_with_expiration(
            u64::MAX,
            Some(clock.slot),
            Some(clock.unix_timestamp as u64),
        ));

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        let mut out_amount = 0;
        let mut in_amount = 0;
        let mut not_enough_liquidity = false;
        let mut best_price: Option<Decimal> = None;

        let ladder = self
            .ladder
            .as_ref()
            .context("Market has not been updated")?;
        if quote_params.input_mint == self.base_mint {
            let mut base_lot_budget = quote_params
                .in_amount
                .checked_div(self.base_lot_size)
                .context("division failed")?;
            let initial_base_lot_budget = base_lot_budget;
            for LadderOrder {
                price_in_ticks,
                size_in_base_lots,
            } in ladder.bids.iter()
            {
                if base_lot_budget == 0 {
                    break;
                }
                let base_lots = size_in_base_lots.min(&base_lot_budget);
                let filled_amount = price_in_ticks
                    .checked_mul(*base_lots)
                    .context("multiply overflow")?
                    .checked_mul(self.tick_size_in_quote_lots_per_base_unit_per_tick)
                    .context("multiply overflow")?
                    .checked_mul(self.quote_lot_size)
                    .context("multiply overflow")?
                    .checked_div(self.base_lots_per_base_unit)
                    .context("division failed")?;

                if best_price.is_none() {
                    let in_amount_for_level = base_lots
                        .checked_mul(self.base_lot_size)
                        .context("multiply overflow")?;
                    best_price = Some(
                        PhoenixAmm::compute_decimal_div(filled_amount, in_amount_for_level)
                            .context("Cannot compute best price")?,
                    );
                }
                out_amount += filled_amount;
                base_lot_budget = base_lot_budget.saturating_sub(*base_lots);
            }
            in_amount = (initial_base_lot_budget - base_lot_budget)
                .checked_mul(self.base_lot_size)
                .context("multiply overflow")?;
            if base_lot_budget > 0 {
                not_enough_liquidity = true;
            }
        } else {
            let mut quote_lot_budget = quote_params
                .in_amount
                .checked_div(self.quote_lot_size)
                .context("division failed")?;
            let initial_quote_lot_budget = quote_lot_budget;
            for LadderOrder {
                price_in_ticks,
                size_in_base_lots,
            } in ladder.asks.iter()
            {
                if quote_lot_budget == 0 {
                    break;
                }
                let purchasable_base_lots = quote_lot_budget
                    .checked_mul(self.base_lots_per_base_unit)
                    .context("multiple overflow")?
                    .checked_div(self.tick_size_in_quote_lots_per_base_unit_per_tick)
                    .context("division failed")?
                    .checked_div(*price_in_ticks)
                    .context("division failed")?;

                let base_lots: u64;
                let quote_lots: u64;
                if size_in_base_lots > &purchasable_base_lots {
                    base_lots = purchasable_base_lots;
                    quote_lots = quote_lot_budget;
                } else {
                    base_lots = *size_in_base_lots;
                    quote_lots = price_in_ticks
                        .checked_mul(base_lots)
                        .context("multiple overflow")?
                        .checked_mul(self.tick_size_in_quote_lots_per_base_unit_per_tick)
                        .context("multiple overflow")?
                        .checked_div(self.base_lots_per_base_unit)
                        .context("division failed")?;
                }
                let filled_amount = base_lots
                    .checked_mul(self.base_lot_size)
                    .context("multiple overflow")?;

                if best_price.is_none() {
                    let in_amount_for_level = quote_lots
                        .checked_mul(self.quote_lot_size)
                        .context("multiple overflow")?;
                    best_price = Some(
                        PhoenixAmm::compute_decimal_div(filled_amount, in_amount_for_level)
                            .context("Cannot compute price impact")?,
                    )
                }

                out_amount += filled_amount;
                quote_lot_budget = quote_lot_budget.saturating_sub(quote_lots);
            }
            in_amount = (initial_quote_lot_budget - quote_lot_budget)
                .checked_div(self.quote_lot_size)
                .context("division failed")?;
            if quote_lot_budget > 0 {
                not_enough_liquidity = true;
            }
        };

        // Not 100% accurate, but it's a reasonable enough approximation
        let out_amount_after_fees = out_amount
            .checked_mul(10_000 - self.taker_fee_bps)
            .context("multiply overflow")?
            .checked_div(10_000)
            .context("division failed")?;
        let fee_amount = out_amount - out_amount_after_fees;

        let price_impact_pct = if quote_params.in_amount > 0 {
            if let Some(best_price) = best_price {
                let price = PhoenixAmm::compute_decimal_div(out_amount, quote_params.in_amount)
                    .context("Cannot compute price")?;
                PhoenixAmm::compute_price_impact_pct(price, best_price)
                    .context("Cannot compute price impact")?
            } else {
                dec!(1)
            }
        } else {
            dec!(1)
        };

        Ok(Quote {
            not_enough_liquidity,
            in_amount,
            out_amount: out_amount_after_fees,
            fee_amount,
            fee_mint: quote_params.output_mint, // Technically quote_mint but fee is estimated on the output amount
            fee_pct: self.fee_pct,
            price_impact_pct,
            ..Quote::default()
        })
    }

    fn get_swap_leg_and_account_metas(
        &self,
        swap_params: &SwapParams,
    ) -> Result<SwapAndAccountMetas> {
        let SwapParams {
            destination_mint,
            source_mint,
            user_destination_token_account,
            user_source_token_account,
            user_transfer_authority,
            ..
        } = swap_params;

        let log_authority = Pubkey::find_program_address(&["log".as_ref()], &ID).0;

        let (side, base_account, quote_account) = if source_mint == &self.base_mint {
            ensure!(destination_mint == &self.quote_mint, "Invalid quote mint");
            (
                Side::Ask,
                *user_source_token_account,
                *user_destination_token_account,
            )
        } else {
            ensure!(destination_mint == &self.base_mint, "Invalid quote mint");
            (
                Side::Bid,
                *user_destination_token_account,
                *user_source_token_account,
            )
        };

        let base_vault = Pubkey::find_program_address(
            &[b"vault", self.market_key.as_ref(), self.base_mint.as_ref()],
            &ID,
        )
        .0;

        let quote_vault = Pubkey::find_program_address(
            &[b"vault", self.market_key.as_ref(), self.quote_mint.as_ref()],
            &ID,
        )
        .0;

        let account_metas = PhoenixSwap {
            swap_program: ID,
            market: self.key(),
            log_authority,
            trader: *user_transfer_authority,
            base_account,
            quote_account,
            base_vault,
            quote_vault,
            token_program: spl_token::ID,
        }
        .to_account_metas(None);

        Ok(SwapAndAccountMetas {
            swap: Swap::Phoenix { side },
            account_metas,
        })
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }
}

#[test]
fn test_jupiter_phoenix_integration() {
    use crate::test_harness::AmmTestHarness;
    use solana_sdk::pubkey;
    use solana_sdk::pubkey::Pubkey;

    const SOL_USDC_MARKET: Pubkey = pubkey!("14CAwu3LiBBk5fcHGdTsFyVxDwvpgFiSfDwgPJxECcE5");

    let test_harness = AmmTestHarness::new();
    let keyed_account = test_harness.get_keyed_account(SOL_USDC_MARKET).unwrap();
    let mut phoenix_amm: PhoenixAmm = PhoenixAmm::from_keyed_account(&keyed_account).unwrap();

    test_harness.update_amm(&mut phoenix_amm);

    let in_amount = 1_000_000_000_000;
    println!(
        "Getting quote for selling {} SOL",
        in_amount as f64 / 10.0_f64.powf(phoenix_amm.get_base_decimals() as f64)
    );
    let quote = phoenix_amm
        .quote(&QuoteParams {
            /// 1 SOL
            in_amount,
            input_mint: phoenix_amm.base_mint,
            output_mint: phoenix_amm.quote_mint,
        })
        .unwrap();

    let Quote { out_amount, .. } = quote;

    println!(
        "Quote result: {:?}",
        out_amount as f64 / 10.0_f64.powf(phoenix_amm.get_quote_decimals() as f64)
    );

    let in_amount = out_amount;

    println!(
        "Getting quote for buying SOL with {} USDC",
        in_amount as f64 / 10.0_f64.powf(phoenix_amm.get_quote_decimals() as f64)
    );
    let quote = phoenix_amm
        .quote(&QuoteParams {
            in_amount,
            input_mint: phoenix_amm.quote_mint,
            output_mint: phoenix_amm.base_mint,
        })
        .unwrap();

    let Quote { out_amount, .. } = quote;

    println!(
        "Quote result: {:?}",
        out_amount as f64 / 10.0_f64.powf(phoenix_amm.get_base_decimals() as f64)
    );
}
