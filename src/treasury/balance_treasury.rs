use easy_ml::matrices::Matrix;
use near_sdk::{require, ONE_NEAR, ONE_YOCTO};
use partial_min_max::{max, min};
use rand::{rngs::StdRng, Rng, SeedableRng};

use crate::*;

use super::ft::{ext_ft, REF_DEPOSIT_ACTION};
use super::gas::*;
use super::pool::{Pool, USDT_DECIMALS};
use super::ref_finance::*;

const NEAR_DECIMALS: u8 = 24;

// 50% slippage: minimizing chance to get failed but not too much.
const SWAP_SLIPPAGE: f64 = 0.5;

struct TreasuryConfig {
    pub wrap_id: &'static str,
    pub swap_pool_id: u64,
}

const CONFIG: TreasuryConfig = if cfg!(feature = "mainnet") {
    TreasuryConfig {
        wrap_id: "wrap.near",
        swap_pool_id: 4,
    }
} else if cfg!(feature = "testnet") {
    TreasuryConfig {
        wrap_id: "wrap.testnet",
        swap_pool_id: 34,
    }
} else {
    TreasuryConfig {
        wrap_id: "wrap.test.near",
        swap_pool_id: 3,
    }
};

#[derive(BorshDeserialize, BorshSerialize, Debug, Serialize, PartialEq, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub enum TreasuryDecision {
    Buy(f64),
    Sell(f64),
    DoNothing,
}

impl std::fmt::Display for TreasuryDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TreasuryDecision::Buy(amount) => {
                write!(f, "Treasury decision is to buy ${} USDT", amount)
            }
            TreasuryDecision::Sell(amount) => {
                write!(f, "Treasury decision is to sell ${} USDT", amount)
            }
            TreasuryDecision::DoNothing => write!(f, "Treasury decision is to do nothing"),
        }
    }
}

#[near_bindgen]
impl Contract {
    #[payable]
    pub fn balance_treasury(
        &mut self,
        pool_id: u64,
        limits: Option<[u64; 2]>,
        execute: Option<bool>,
    ) -> Promise {
        self.assert_owner_or_guardian();

        // Buy case: 2 yoctoNEAR, sell case: 3 yoctoNEAR.
        require!(
            env::attached_deposit() == 3 * ONE_YOCTO,
            "3 yoctoNEAR of attached deposit is required"
        );

        let decision_limit = if let Some(range) = limits {
            let min = range[0];
            let max = range[1];

            require!(min <= max, "`limits` must be in [min; max] format");

            let mut rng = StdRng::from_seed(env::random_seed_array());
            Some(rng.gen_range(min..max))
        } else {
            None
        };

        let treasury = self.treasury.get().expect("Valid treasury");
        if let Err(_) = treasury.cache.collect(env::block_timestamp()) {
            env::panic_str("Treasury cache is not warmed up. Use `warmup`.");
        }

        let pool = Pool::from_config_with_assert(pool_id);

        // Start with figuring out USDT part of reserve.
        ext_ref_finance::get_pool_shares(
            pool.id,
            env::current_account_id(),
            pool.ref_id.clone(),
            NO_DEPOSIT,
            GAS_FOR_GET_SHARES,
        )
        .then(ext_self::predict_remove_liquidity(
            pool.id,
            env::current_account_id(),
            NO_DEPOSIT,
            GAS_SURPLUS + GAS_FOR_PREDICT_REMOVE_LIQUIDITY,
        ))
        .and(ext_ref_finance::get_stable_pool(
            pool.id,
            pool.ref_id,
            NO_DEPOSIT,
            GAS_FOR_GET_STABLE_POOL,
        ))
        .then(ext_self::handle_start_treasury_balancing(
            pool.id,
            decision_limit,
            execute.unwrap_or(false),
            env::current_account_id(),
            env::attached_deposit(),
            GAS_SURPLUS * 6
                + GAS_FOR_REMOVE_LIQUIDITY
                + GAS_FOR_SWAP
                + GAS_FOR_WITHDRAW * 2
                + GAS_FOR_NEAR_WITHDRAW
                + GAS_FOR_FINISH_BURNING,
        ))
    }

    pub fn warmup(&mut self) -> Promise {
        Oracle::get_exchange_rate_promise().then(ext_self::handle_exchange_rate_cache(
            env::current_account_id(),
            NO_DEPOSIT,
            GAS_FOR_HANDLE_EXCHANGE_RATE,
        ))
    }
}

#[ext_contract(ext_self)]
trait SelfHandler {
    #[private]
    #[payable]
    fn handle_start_treasury_balancing(
        &mut self,
        pool_id: u64,
        decision_limit: Option<u64>,
        execute: bool,
        #[callback] predicted_amounts: Vec<U128>,
        #[callback] info: StablePoolInfo,
    ) -> PromiseOrValue<()>;

    #[private]
    #[payable]
    fn handle_withdraw_after_swap(
        &mut self,
        pool_id: u64,
        usn_amount: U128,
        #[callback] wrap_amount: U128,
    ) -> Promise;

    #[private]
    #[payable]
    fn handle_liquidity_after_swap(&mut self, pool_id: u64, #[callback] amount: U128) -> Promise;

    #[private]
    fn handle_exchange_rate_cache(&mut self, #[callback] price: PriceData);

    #[private]
    fn predict_remove_liquidity(&self, pool_id: u64, #[callback] shares: U128) -> Promise;

    #[private]
    fn finish_withdraw_with_burn(&mut self, amount: U128);
}

trait SelfHandler {
    fn handle_start_treasury_balancing(
        &mut self,
        pool_id: u64,
        decision_limit: Option<u64>,
        execute: bool,
        predicted_amounts: Vec<U128>,
        info: StablePoolInfo,
    ) -> PromiseOrValue<()>;

    fn handle_withdraw_after_swap(
        &mut self,
        pool_id: u64,
        usn_amount: U128,
        wrap_amount: U128,
    ) -> Promise;

    fn handle_liquidity_after_swap(&mut self, pool_id: u64, amount: U128) -> Promise;

    fn handle_exchange_rate_cache(&mut self, price: PriceData);

    fn predict_remove_liquidity(&self, pool_id: u64, shares: U128) -> Promise;

    fn finish_withdraw_with_burn(&mut self, amount: U128);
}

enum Extract {
    Usn,
    Other,
}

fn extract_amount(pool: &Pool, amounts: &Vec<U128>, extract: Extract) -> U128 {
    pool.tokens
        .iter()
        .zip(amounts)
        .find_map(|(token_id, amount)| match extract {
            Extract::Usn => {
                if token_id == &env::current_account_id() {
                    Some(amount.clone())
                } else {
                    None
                }
            }
            _ => {
                if token_id != &env::current_account_id() {
                    Some(amount.clone())
                } else {
                    None
                }
            }
        })
        .unwrap()
}

#[near_bindgen]
impl SelfHandler for Contract {
    #[private]
    #[payable]
    fn handle_start_treasury_balancing(
        &mut self,
        pool_id: u64,
        decision_limit: Option<u64>,
        execute: bool,
        #[callback] predicted_amounts: Vec<U128>,
        #[callback] info: StablePoolInfo,
    ) -> PromiseOrValue<()> {
        let pool = Pool::from_config_with_assert(pool_id);

        require!(
            predicted_amounts.len() == 2,
            "A pool of 2 tokens is required"
        );

        let treasury = self.treasury.get().expect("Valid treasury");

        // Prepare input data to make decision about balancing.

        // 1. NEAR/USDT exchange rates.
        let (time_points, exchange_rates) = match treasury.cache.collect(env::block_timestamp()) {
            Ok((time_points, exchange_rates)) => (time_points, exchange_rates),
            Err(_) => env::panic_str("Treasury cache is not in a valid state."),
        };

        // 2. NEAR part of USN reserve in NEAR.
        let near = env::account_balance() - env::attached_deposit();

        // 3. Total value of circulating USN.
        let usn =
            self.token.ft_total_supply().0 - extract_amount(&pool, &info.amounts, Extract::Usn).0;

        // 4. USDT reserve.
        let usdt = extract_amount(&pool, &predicted_amounts, Extract::Other).0;

        // Convert everything into floats.
        let near = near as f64 / ONE_NEAR as f64;
        let usn = usn as f64 / 10f64.powi(USN_DECIMALS as i32);
        let last_exch_rate = *exchange_rates.last().unwrap();
        let usdt = usdt as f64 / 10f64.powi(USDT_DECIMALS as i32);
        let limit = decision_limit.map(|x| x as f64);

        // Make a decision.
        let decision = make_treasury_decision(exchange_rates, time_points, near, usn, usdt, limit);

        env::log_str(format!("{}", decision).as_str());

        if execute {
            match decision {
                TreasuryDecision::DoNothing => PromiseOrValue::Value(()),
                TreasuryDecision::Buy(f_amount) => buy(pool.id, f_amount, last_exch_rate).into(),
                TreasuryDecision::Sell(f_amount) => sell(pool.id, f_amount, last_exch_rate).into(),
            }
        } else {
            env::log_str("Execution bypassed");
            PromiseOrValue::Value(())
        }
    }

    #[private]
    #[payable]
    fn handle_withdraw_after_swap(
        &mut self,
        pool_id: u64,
        usn_amount: U128,
        #[callback] wrap_amount: U128,
    ) -> Promise {
        let wrap_id: AccountId = CONFIG.wrap_id.parse().unwrap();
        let usn_id = env::current_account_id();
        let pool = Pool::from_config_with_assert(pool_id);

        ext_ref_finance::withdraw(
            wrap_id.clone(),
            wrap_amount,
            None,
            pool.ref_id.clone(),
            ONE_YOCTO,
            GAS_FOR_WITHDRAW,
        )
        .then(ext_ft::near_withdraw(
            wrap_amount,
            wrap_id,
            ONE_YOCTO,
            GAS_FOR_NEAR_WITHDRAW,
        ))
        .then(ext_ref_finance::withdraw(
            usn_id.clone(),
            usn_amount,
            None,
            pool.ref_id,
            1,
            GAS_FOR_WITHDRAW,
        ))
        .then(ext_self::finish_withdraw_with_burn(
            usn_amount,
            usn_id,
            NO_DEPOSIT,
            GAS_FOR_FINISH_BURNING,
        ))
    }

    #[private]
    #[payable]
    fn handle_liquidity_after_swap(&mut self, pool_id: u64, #[callback] amount: U128) -> Promise {
        let pool = Pool::from_config_with_assert(pool_id);

        let add_amounts = pool
            .tokens
            .iter()
            .map(|token_id| {
                if token_id == &env::current_account_id() {
                    U128(0u128)
                } else {
                    amount
                }
            })
            .collect();

        let min_shares = U128::from(0u128);

        ext_ref_finance::add_stable_liquidity(
            pool.id,
            add_amounts,
            min_shares,
            pool.ref_id,
            ONE_YOCTO,
            GAS_FOR_ADD_LIQUIDITY,
        )
    }

    #[private]
    fn handle_exchange_rate_cache(&mut self, #[callback] price: PriceData) {
        let mut treasury = self.treasury.take().unwrap();
        let rate: ExchangeRate = price.into();
        let rate = rate.multiplier() as f64 / 10f64.powi((rate.decimals() - NEAR_DECIMALS) as i32);
        treasury.cache.append(env::block_timestamp(), rate);
        self.treasury.replace(&treasury);
    }

    #[private]
    fn predict_remove_liquidity(&self, pool_id: u64, #[callback] shares: U128) -> Promise {
        let pool = Pool::from_config_with_assert(pool_id);
        ext_ref_finance::predict_remove_liquidity(
            pool.id,
            shares,
            pool.ref_id,
            NO_DEPOSIT,
            GAS_FOR_PREDICT_REMOVE_LIQUIDITY,
        )
    }

    #[private]
    fn finish_withdraw_with_burn(&mut self, amount: U128) {
        if is_promise_success() {
            self.token
                .internal_withdraw(&env::current_account_id(), amount.into());
            event::emit::ft_burn(&env::current_account_id(), amount.into(), None);
        }
    }
}

fn buy(pool_id: u64, amount: f64, exchange_rate: f64) -> Promise {
    let wrap_id: AccountId = CONFIG.wrap_id.parse().unwrap();
    let pool = Pool::from_config_with_assert(pool_id);
    let near = ((amount / exchange_rate) * ONE_NEAR as f64) as u128;
    let min_amount = (amount * SWAP_SLIPPAGE * 10f64.powi(USDT_DECIMALS as i32)) as u128;

    env::log_str(&format!("Trying to wrap {} NEAR", near));

    let usdt_name = pool
        .tokens
        .iter()
        .find(|&token_id| token_id != &env::current_account_id())
        .unwrap();

    let swap_action = SwapAction {
        pool_id: CONFIG.swap_pool_id,
        amount_in: Some(near.into()),
        token_in: wrap_id.clone(),
        token_out: usdt_name.clone(),
        min_amount_out: U128(min_amount),
    };

    ext_ft::near_deposit(wrap_id.clone(), near, GAS_FOR_NEAR_DEPOSIT)
        .then(ext_ft::ft_transfer_call(
            pool.ref_id.clone(),
            near.into(),
            None,
            REF_DEPOSIT_ACTION.into(),
            wrap_id,
            ONE_YOCTO,
            GAS_FOR_FT_TRANSFER_CALL,
        ))
        .then(ext_ref_finance::swap(
            vec![swap_action],
            None,
            pool.ref_id.clone(),
            NO_DEPOSIT,
            GAS_FOR_SWAP,
        ))
        .then(ext_self::handle_liquidity_after_swap(
            pool.id,
            env::current_account_id(),
            ONE_YOCTO,
            GAS_SURPLUS + GAS_FOR_ADD_LIQUIDITY,
        ))
}

fn sell(pool_id: u64, amount: f64, exchange_rate: f64) -> Promise {
    let wrap_id = CONFIG.wrap_id.parse().unwrap();
    let pool = Pool::from_config_with_assert(pool_id);
    let min_amount =
        ((amount * SWAP_SLIPPAGE / exchange_rate) * 10f64.powi(USN_DECIMALS as i32)) as u128;

    let remove_amounts: Vec<U128> = pool
        .decimals
        .iter()
        .map(|decimals| U128((amount * 10f64.powi(*decimals as i32)) as u128))
        .collect();

    let usn_amount = extract_amount(&pool, &remove_amounts, Extract::Usn);

    let (usdt_name, usdt_amount) = pool
        .tokens
        .into_iter()
        .zip(remove_amounts.clone().into_iter())
        .find(|(token_id, _amount)| token_id != &env::current_account_id())
        .unwrap();

    let swap_action = SwapAction {
        pool_id: CONFIG.swap_pool_id,
        amount_in: Some(usdt_amount),
        token_in: usdt_name,
        token_out: wrap_id,
        min_amount_out: min_amount.into(),
    };

    let max_burn_shares = U128(u128::MAX); // TODO: Any limits?

    ext_ref_finance::remove_liquidity_by_tokens(
        pool.id,
        remove_amounts.clone(),
        max_burn_shares,
        pool.ref_id.clone(),
        ONE_YOCTO,
        GAS_FOR_REMOVE_LIQUIDITY,
    )
    .then(ext_ref_finance::swap(
        vec![swap_action],
        None,
        pool.ref_id,
        NO_DEPOSIT,
        GAS_FOR_SWAP,
    ))
    .then(ext_self::handle_withdraw_after_swap(
        pool.id,
        usn_amount,
        env::current_account_id(),
        2 * ONE_YOCTO,
        GAS_SURPLUS * 3 + GAS_FOR_WITHDRAW * 2 + GAS_FOR_NEAR_WITHDRAW + GAS_FOR_FINISH_BURNING,
    ))
}

fn make_treasury_decision(
    exchange_rates: Vec<f64>,
    time_points: Vec<f64>,
    near: f64,
    usn: f64,
    usdt: f64,
    limit: Option<f64>,
) -> TreasuryDecision {
    // 1. Set constant values for further calculations
    const M: i32 = 4;
    const N_DN: f64 = 0.25;
    const U_UP: f64 = 1.1;
    const U_DN: f64 = 1.;
    const P_DN: f64 = 0.6;
    const P_UP: f64 = 0.7;
    const T_BUY_MIN: f64 = 1000.;
    const T_SELL_MIN: f64 = 1000.;
    const T_BUY_STEP: f64 = 3_000_000.;
    const T_SELL_STEP: f64 = 3_000_000.;
    const T_0: f64 = 0.;

    let n = near;
    let q = usn;
    let u = usdt;

    debug_assert_eq!(exchange_rates.len(), time_points.len());
    debug_assert_eq!(exchange_rates.len(), 8);

    // 2. Set NER = ER[t − 0] = V8
    let n_er = exchange_rates.last().unwrap();

    // 3. Make the data smoothing with moving average
    let mut x: Vec<f64> = Vec::new();
    let mut y: Vec<f64> = Vec::new();
    for k in 1..7 {
        x.push((time_points[k - 1] + time_points[k] + time_points[k + 1]) / 3.);
        y.push((exchange_rates[k - 1] + exchange_rates[k] + exchange_rates[k + 1]) / 3.);
    }

    // 4. Fit a quadratic trend into the 6 NEAR/USDT smoothed exchange rate values collected using OLS:
    let x: Matrix<f64> = Matrix::column(x);
    let y: Matrix<f64> = Matrix::column(y);

    let mut basis = x.clone();
    basis.insert_column(0, 1.0);
    basis.insert_column_with(2, x.column_iter(0).map(|x| x * x));

    let w = (basis.transpose() * &basis).inverse().unwrap() * (basis.transpose() * &y);

    // 5. Get coefficients a, b, c and R2 for this trend
    let a = w.get(2, 0);
    let b = w.get(1, 0);
    let c = w.get(0, 0);

    // Stot = ∑((Y − Y _mean)2)
    let er_mean: f64 = exchange_rates.clone().iter().sum::<f64>() / exchange_rates.len() as f64;

    let s_tot = exchange_rates
        .clone()
        .iter()
        .map(|er| (er - er_mean).powi(2))
        .sum::<f64>();

    // Sres = ∑(Vk − (a · Tk^2 + b · Tk + c))2
    let mut s_res: f64 = 0.;
    for n in 0..exchange_rates.len() {
        s_res +=
            (exchange_rates[n] - (time_points[n].powi(2) * a + time_points[n] * b + c)).powi(2);
    }

    // R2 = 1 − Sres/Stot
    let r_squared = 1. - s_res / s_tot;

    // 5. Calculate coefficient C
    // C = sign(a) · R^2/(t0 + b/2a)^m + 1)
    let c = f64::signum(a) * r_squared / ((T_0 + b / (2. * a)).powi(M) + 1.);

    if N_DN * q - n_er * n >= 0. {
        let r_sell = min(
            min(min(N_DN * q - n_er * n, T_SELL_STEP), u),
            limit.unwrap_or(T_SELL_STEP),
        );

        if r_sell >= T_SELL_MIN {
            TreasuryDecision::Sell(r_sell)
        } else {
            TreasuryDecision::DoNothing
        }
    } else if N_DN * q - n_er * n < 0. && c > 0. {
        let u_sell = max(c * (u - min(P_UP * (u + n_er * n), U_UP * q)), 0.);

        let r_sell = min(
            min(min(u_sell, T_SELL_STEP), u),
            limit.unwrap_or(T_SELL_STEP),
        );

        if r_sell >= T_SELL_MIN {
            TreasuryDecision::Sell(r_sell)
        } else {
            TreasuryDecision::DoNothing
        }
    } else {
        let u_buy = c * min(u - min(P_DN * (u + n_er * n), U_DN * q), 0.);

        let r_buy = min(
            min(min(u_buy, T_BUY_STEP), n_er * n),
            limit.unwrap_or(T_BUY_STEP),
        );

        if r_buy >= T_BUY_MIN {
            TreasuryDecision::Buy(r_buy)
        } else {
            TreasuryDecision::DoNothing
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn test_make_treasury_decision_sell() {
        let treasury_decision = make_treasury_decision(
            vec![6.615, 6.62, 6.628, 6.623, 6.578, 6.6, 6.577, 6.611],
            vec![-7., -6., -5., -4., -3., -2., -1., -0.],
            191937460.53121,
            1241195491.76577,
            1367351872.04769,
            None,
        );

        assert_eq!(
            treasury_decision,
            TreasuryDecision::Sell(23604.588213058174)
        );
    }

    #[test]
    fn test_make_treasury_decision_sell_with_limit() {
        let treasury_decision = make_treasury_decision(
            vec![6.615, 6.62, 6.628, 6.623, 6.578, 6.6, 6.577, 6.611],
            vec![-7., -6., -5., -4., -3., -2., -1., -0.],
            191937460.53121,
            1241195491.76577,
            1367351872.04769,
            Some(20000.),
        );

        assert_eq!(treasury_decision, TreasuryDecision::Sell(20000.));
    }

    #[test]
    fn test_make_treasury_decision_do_nothing() {
        let treasury_decision = make_treasury_decision(
            vec![
                5.9519, 5.9222, 5.9189, 5.9242, 5.9194, 5.9173, 5.8818, 5.8741,
            ],
            vec![-7., -6., -5., -4., -3., -2., -1., -0.],
            167242050.870139,
            1001497797.34406,
            1000522964.94309,
            None,
        );

        assert_eq!(treasury_decision, TreasuryDecision::DoNothing);
    }

    #[test]
    fn test_make_treasury_decision_buy() {
        let treasury_decision = make_treasury_decision(
            vec![
                5.6584, 5.809, 5.7635, 5.8331, 5.8555, 5.8643, 5.8565, 5.8699,
            ],
            vec![-7., -6., -5., -4., -3., -2., -1., -0.],
            167270746.338665,
            1001096736.9184,
            1000039562.72316,
            None,
        );

        assert_eq!(treasury_decision, TreasuryDecision::Buy(207013.8891493543));
    }
}
