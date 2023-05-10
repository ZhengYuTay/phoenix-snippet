import { AccountInfo, PublicKey } from "@solana/web3.js";
import {
  MarketData,
  deserializeMarketData,
  Ladder,
  getMarketLadder,
} from "@jup-ag/phoenix-sdk";
import {
  AccountInfoMap,
  Amm,
  QuoteParams,
  SwapParams,
  SwapLegAndAccounts,
} from "../../amm";
import JSBI from "jsbi";
import { ZERO } from "@jup-ag/math";
import { PHOENIX_PROGRAM_ID } from "@jup-ag/common";
import Decimal from "decimal.js";

const FEE_DENOMINATOR = JSBI.BigInt(10_000);

export class PhoenixAmm implements Amm {
  id: string;
  label = "Phoenix" as const;
  shouldPrefetch = false;
  exactOutputSupported = false;
  hasDynamicAccounts = false;

  private marketData: MarketData;
  ladder: Ladder;
  private outAmountWithoutFeesMultiplier: JSBI;
  private baseLotsPerBaseUnit: JSBI;
  private baseLotSize: JSBI;
  private quoteLotSize: JSBI;
  private tickSizeInQuoteLotsPerBaseUnitPerTick: JSBI;

  constructor(private address: PublicKey, accountInfo: AccountInfo<Buffer>) {
    this.id = address.toBase58();
    this.marketData = deserializeMarketData(accountInfo.data);
    this.ladder = getMarketLadder(this.marketData, -1);

    this.outAmountWithoutFeesMultiplier = JSBI.BigInt(
      10_000 - this.marketData.takerFeeBps
    );
    this.baseLotsPerBaseUnit = JSBI.BigInt(this.marketData.baseLotsPerBaseUnit);

    const header = this.marketData.header;
    this.baseLotSize = JSBI.BigInt(header.baseLotSize.toString());
    this.quoteLotSize = JSBI.BigInt(header.quoteLotSize.toString());

    this.tickSizeInQuoteLotsPerBaseUnitPerTick = JSBI.divide(
      JSBI.BigInt(header.tickSizeInQuoteAtomsPerBaseUnit.toString()),
      this.quoteLotSize
    );
  }

  getAccountsForUpdate(): PublicKey[] {
    return [this.address];
  }

  update(accountInfoMap: AccountInfoMap): void {
    const marketAccountInfo = accountInfoMap.get(this.address.toBase58());
    if (!marketAccountInfo)
      throw new Error(`Missing market accountInfo ${this.address.toBase58()}`);
    this.marketData = deserializeMarketData(marketAccountInfo.data);
    this.ladder = getMarketLadder(this.marketData, -1);
  }

  private JSBImin(x: JSBI, y: JSBI) {
    return JSBI.lessThan(x, y) ? x : y;
  }

  private computeQuote({
    sourceMint,
    amount,
  }: {
    sourceMint: PublicKey;
    amount: JSBI;
  }) {
    let outAmount = JSBI.BigInt(0);
    let inAmount = ZERO;
    let notEnoughLiquidity = false;
    let bestPriceDecimal: Decimal | undefined;
    if (sourceMint.equals(this.marketData.header.baseParams.mintKey)) {
      let baseLotBudget = JSBI.divide(amount, this.baseLotSize);
      const initialBaseLotBudget = JSBI.BigInt(baseLotBudget);
      for (const [priceInTicks, sizeInBaseLots] of this.ladder.bids) {
        if (JSBI.lessThanOrEqual(baseLotBudget, ZERO)) {
          break;
        }
        const priceInTicksJSBI = JSBI.BigInt(priceInTicks.toString());
        const sizeInBaseLotsJSBI = JSBI.BigInt(sizeInBaseLots.toString());

        const baseLots = this.JSBImin(sizeInBaseLotsJSBI, baseLotBudget);
        const filledAmount = JSBI.divide(
          JSBI.multiply(
            JSBI.multiply(
              JSBI.multiply(priceInTicksJSBI, baseLots),
              this.tickSizeInQuoteLotsPerBaseUnitPerTick
            ),
            this.quoteLotSize
          ),
          this.baseLotsPerBaseUnit
        );

        if (!bestPriceDecimal) {
          const inAmountForLevel = JSBI.multiply(baseLots, this.baseLotSize);
          bestPriceDecimal = new Decimal(filledAmount.toString()).div(
            inAmountForLevel.toString()
          );
        }

        outAmount = JSBI.add(outAmount, filledAmount);
        baseLotBudget = JSBI.subtract(baseLotBudget, baseLots);
      }
      inAmount = JSBI.multiply(
        JSBI.subtract(initialBaseLotBudget, baseLotBudget),
        this.baseLotSize
      );
      if (JSBI.greaterThan(baseLotBudget, ZERO)) {
        notEnoughLiquidity = true;
      }
    } else {
      let quoteLotBudget = JSBI.divide(amount, this.quoteLotSize);
      const initialQuoteLotBudget = JSBI.BigInt(quoteLotBudget);
      for (const [priceInTicks, sizeInBaseLots] of this.ladder.asks) {
        if (JSBI.lessThanOrEqual(quoteLotBudget, ZERO)) {
          break;
        }
        const priceInTicksJSBI = JSBI.BigInt(priceInTicks.toString());
        const sizeInBaseLotsJSBI = JSBI.BigInt(sizeInBaseLots.toString());

        const purchasableBaseLots = JSBI.divide(
          JSBI.divide(
            JSBI.multiply(quoteLotBudget, this.baseLotsPerBaseUnit),
            this.tickSizeInQuoteLotsPerBaseUnitPerTick
          ),
          priceInTicksJSBI
        );

        let baseLots: JSBI;
        let quoteLots: JSBI;
        if (JSBI.greaterThan(sizeInBaseLotsJSBI, purchasableBaseLots)) {
          baseLots = purchasableBaseLots;
          quoteLots = quoteLotBudget;
        } else {
          baseLots = sizeInBaseLotsJSBI;
          quoteLots = JSBI.divide(
            JSBI.multiply(
              JSBI.multiply(priceInTicksJSBI, baseLots),
              this.tickSizeInQuoteLotsPerBaseUnitPerTick
            ),
            this.baseLotsPerBaseUnit
          );
        }
        const filledAmount = JSBI.multiply(baseLots, this.baseLotSize);

        if (!bestPriceDecimal) {
          const inAmountForLevel = JSBI.multiply(quoteLots, this.quoteLotSize);
          bestPriceDecimal = new Decimal(filledAmount.toString()).div(
            inAmountForLevel.toString()
          );
        }

        outAmount = JSBI.add(outAmount, filledAmount);
        quoteLotBudget = JSBI.subtract(quoteLotBudget, quoteLots);
      }
      inAmount = JSBI.multiply(
        JSBI.subtract(initialQuoteLotBudget, quoteLotBudget),
        this.quoteLotSize
      );
      if (JSBI.greaterThan(quoteLotBudget, ZERO)) {
        notEnoughLiquidity = true;
      }
    }

    const outAmountAfterFees = this.computAmountAfterFees(outAmount);
    const feeAmount = JSBI.subtract(outAmount, outAmountAfterFees);
    // price uses the input amount rather than the consumed amount
    const priceDecimal = new Decimal(outAmount.toString()).div(
      amount.toString()
    );
    if (!bestPriceDecimal) throw new Error("No best price");

    const priceImpactPct = bestPriceDecimal
      .sub(priceDecimal)
      .div(bestPriceDecimal)
      .toNumber();
    return {
      notEnoughLiquidity,
      inAmount,
      outAmount: outAmountAfterFees,
      feeAmount,
      priceImpactPct,
    };
  }

  computAmountAfterFees(outAmount: JSBI) {
    return JSBI.divide(
      JSBI.multiply(outAmount, this.outAmountWithoutFeesMultiplier),
      FEE_DENOMINATOR
    );
  }

  getQuote({ sourceMint, amount }: QuoteParams) {
    const {
      notEnoughLiquidity,
      inAmount,
      outAmount,
      feeAmount,
      priceImpactPct,
    } = this.computeQuote({
      sourceMint,
      amount,
    });

    return {
      notEnoughLiquidity,
      inAmount,
      outAmount,
      feeAmount,
      feeMint: sourceMint.toBase58(),
      feePct: this.marketData.takerFeeBps / 10_000,
      priceImpactPct,
    };
  }

  getSwapLegAndAccounts(swapParams: SwapParams): SwapLegAndAccounts {
    return createPhoenixSwapLegAndAccounts({
      ...swapParams,
      additionalArgs: {
        logAuthority: PublicKey.findProgramAddressSync(
          [Buffer.from("log")],
          PHOENIX_PROGRAM_ID
        )[0],
        market: this.address,
        baseVault: this.marketData.header.baseParams.vaultKey,
        quoteVault: this.marketData.header.quoteParams.vaultKey,
        baseMint: this.marketData.header.baseParams.mintKey,
      },
    });
  }

  get reserveTokenMints() {
    return [
      this.marketData.header.baseParams.mintKey,
      this.marketData.header.quoteParams.mintKey,
    ];
  }
}

function createPhoenixSwapLegAndAccounts({
  additionalArgs,
  sourceMint,
  userSourceTokenAccount,
  userDestinationTokenAccount,
  userTransferAuthority,
}: {
  additionalArgs: PhoenixInstructionArgs;
} & CreateSwapInstructionParams): SwapLegAndAccounts {
  const { side, baseAccount, quoteAccount } = sourceMint.equals(
    additionalArgs.baseMint
  )
    ? {
        side: Side.Ask,
        baseAccount: userSourceTokenAccount,
        quoteAccount: userDestinationTokenAccount,
      }
    : {
        side: Side.Bid,
        baseAccount: userDestinationTokenAccount,
        quoteAccount: userSourceTokenAccount,
      };

  return [
    SwapLeg.Swap(Swap.Phoenix(side)),
    JUPITER_PROGRAM.instruction.phoenixSwap({
      accounts: {
        swapProgram: PHOENIX_PROGRAM_ID,
        logAuthority: additionalArgs.logAuthority,
        market: additionalArgs.market,
        trader: userTransferAuthority,
        baseAccount,
        quoteAccount,
        baseVault: additionalArgs.baseVault,
        quoteVault: additionalArgs.quoteVault,
        tokenProgram: TOKEN_PROGRAM_ID,
      },
    }).keys,
  ];
}
