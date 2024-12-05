use rand::seq::SliceRandom;
use rand::SeedableRng;
use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke;
use anchor_lang::solana_program::instruction::Instruction;

declare_id!("9CW2nv7psxCDH8Qr2XQGnHxveTYtMU6mHLzD2FXfG4kc");

#[program]
pub mod pixel_card_game {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, start_time: i64, end_time: i64, entry_fee: u64) -> Result<()> {
        if start_time >= end_time {
            return err!(ErrorCode::InvalidStartTime);
        }
        if entry_fee == 0 {
            return err!(ErrorCode::InvalidEntryFee);
        }

        let state = &mut ctx.accounts.state;
        state.admin = ctx.accounts.admin.key();
        state.entry_fee = entry_fee;
        state.start_time = start_time;
        state.end_time = end_time;
        state.leaderboard_size = 3;
        state.finalized = false;
        state.pool = 0;
        state.finalized_timestamp = 0;

        emit!(GameInitialized {
            admin: state.admin,
            entry_fee,
            start_time,
            end_time,
        });

        Ok(())
    }

    pub fn request_randomness(ctx: Context<RequestRandomness>, seed: u64) -> Result<()> {
        let switchboard_vrf_program = ctx.accounts.switchboard_vrf_program.to_account_info();
        let vrf_account = &ctx.accounts.vrf_account;

        let instruction = Instruction {
            program_id: switchboard_vrf_program.key(),
            accounts: vec![
                AccountMeta::new_readonly(vrf_account.key(), false),
                AccountMeta::new(ctx.accounts.admin.key(), true),
            ],
            data: seed.to_le_bytes().to_vec(),
        };

        invoke(
            &instruction,
            &[switchboard_vrf_program, vrf_account.to_account_info(), ctx.accounts.admin.to_account_info()]
        )?;

        emit!(RandomnessRequested { seed });
        Ok(())
    }

    pub fn receive_randomness(ctx: Context<ReceiveRandomness>, randomness: u64) -> Result<()> {
        let player = &mut ctx.accounts.player;

        if player.randomness.is_some() {
            return err!(ErrorCode::RandomnessAlreadyReceived);
        }

        player.randomness = Some(randomness);
        player.deck = shuffle_deck(randomness);

        emit!(RandomnessReceived { randomness });
        Ok(())
    }

    pub fn start_game(ctx: Context<StartGame>, game_id: u64) -> Result<()> {
        let player = &mut ctx.accounts.player;

        if player.daily_games >= 10 {
            return err!(ErrorCode::DailyLimitReached);
        }

        player.daily_games += 1;
        player.start_time = Clock::get()?.unix_timestamp;
        player.game_id = game_id;
        player.finished = false;
        player.multiplier = 1.0;

        emit!(GameStarted { player: player.key(), game_id });
        Ok(())
    }

    pub fn place_bet(ctx: Context<PlaceBet>, bet_type: BetType, side_bet: Option<SideBetType>) -> Result<()> {
        let player = &mut ctx.accounts.player;

        if Clock::get()?.unix_timestamp - player.start_time > 60 {
            return err!(ErrorCode::BetTimeExpired);
        }

        let outcome = resolve_bet(player, &bet_type, side_bet)?;

        if !outcome.correct {
            player.finished = true;
            emit!(GameOver {
                player: player.key(),
                game_id: player.game_id,
                final_multiplier: player.multiplier,
                side_bet_score: player.side_bet_score,
            });
            return err!(ErrorCode::GameOver);
        }

        player.multiplier *= outcome.multiplier_gain;

        if let Some(side_bet_result) = outcome.side_bet_result {
            player.side_bet_score += side_bet_result;
        }

        emit!(BetPlaced {
            player: player.key(),
            game_id: player.game_id,
            bet_type,
            multiplier_gain: outcome.multiplier_gain,
            side_bet_result: outcome.side_bet_result,
        });

        Ok(())
    }

    pub fn finalize_leaderboard(ctx: Context<FinalizeLeaderboard>) -> Result<()> {
        let state = &mut ctx.accounts.state;

        if ctx.accounts.admin.key() != state.admin {
            return err!(ErrorCode::Unauthorized);
        }

        let leaderboard_size = state.leaderboard_size.into();
        state.leaderboard.sort_by(|a, b| b.score.cmp(&a.score));
        state.leaderboard.truncate(leaderboard_size);

        state.finalized = true;
        state.finalized_timestamp = Clock::get()?.unix_timestamp;

        emit!(LeaderboardFinalized {
            timestamp: state.finalized_timestamp,
            leaderboard: state.leaderboard.clone(),
        });

        Ok(())
    }

    pub fn claim_prize(ctx: Context<ClaimPrize>, position: u8) -> Result<()> {
        let state = &mut ctx.accounts.state;

        if !state.finalized {
            return err!(ErrorCode::PrizeWindowExpired);
        }

        if Clock::get()?.unix_timestamp > state.finalized_timestamp + 72 * 3600 {
            return err!(ErrorCode::PrizeWindowExpired);
        }

        if position as usize >= state.leaderboard.len() {
            return err!(ErrorCode::NotOnLeaderboard);
        }

        let prize_percentage: u64 = match position {
            0 => 50_u64,
            1 => 30_u64,
            2 => 20_u64,
            _ => return err!(ErrorCode::NotOnLeaderboard),
        };

        let amount = state
            .pool
            .checked_mul(prize_percentage)
            .and_then(|total| total.checked_div(100))
            .ok_or(ErrorCode::ArithmeticError)?;

        **ctx.accounts.state.to_account_info().try_borrow_mut_lamports()? -= amount;
        **ctx.accounts.player_wallet.to_account_info().try_borrow_mut_lamports()? += amount;

        emit!(PrizeClaimed {
            player: ctx.accounts.player.key(),
            position: position as usize,
            prize: amount,
        });

        Ok(())
    }
}

// Utility Functions
pub fn shuffle_deck(randomness: u64) -> Vec<Card> {
    let suits = ["Hearts", "Diamonds", "Clubs", "Spades"];
    let values = 2..=14;

    let mut deck: Vec<Card> = suits
        .iter()
        .flat_map(|suit| values.clone().map(move |value| Card {
            suit: suit.to_string(),
            value,
        }))
        .collect();

    let mut rng = rand::rngs::StdRng::seed_from_u64(randomness);
    deck.shuffle(&mut rng);

    deck
}

pub fn resolve_bet(player: &mut Player, bet_type: &BetType, side_bet: Option<SideBetType>) -> Result<BetOutcome> {
    if player.deck.is_empty() {
        return err!(ErrorCode::GameOver);
    }

    let current_card = player.deck.remove(0);
    let next_card = player.deck.get(0).cloned().ok_or(ErrorCode::GameOver)?;

    let outcome = match bet_type {
        BetType::High => next_card.value > current_card.value,
        BetType::Low => next_card.value < current_card.value,
    };

    let multiplier_gain = calculate_multiplier_gain(current_card.value, *bet_type);

    let side_bet_result = if let Some(bet) = side_bet {
        match bet {
            SideBetType::Color { red } => {
                let is_red = current_card.suit == "Hearts" || current_card.suit == "Diamonds";
                if red == is_red {
                    Some(1)
                } else {
                    Some(-1)
                }
            }
            SideBetType::Parity { even } => {
                let is_even = current_card.value % 2 == 0;
                if even == is_even {
                    Some(1)
                } else {
                    Some(-1)
                }
            }
        }
    } else {
        None
    };

    Ok(BetOutcome {
        correct: outcome,
        multiplier_gain,
        side_bet_result,
    })
}

fn calculate_multiplier_gain(current_card_value: u8, bet_type: BetType) -> f64 {
    match current_card_value {
        2 => match bet_type {
            BetType::High => 1.2,
            BetType::Low => 4.0,
        },
        3 => match bet_type {
            BetType::High => 1.25,
            BetType::Low => 3.5,
        },
        4 => match bet_type {
            BetType::High => 1.3,
            BetType::Low => 3.0,
        },
        5 => match bet_type {
            BetType::High => 1.35,
            BetType::Low => 2.5,
        },
        6 => match bet_type {
            BetType::High => 1.4,
            BetType::Low => 2.0,
        },
        7 => match bet_type {
            BetType::High => 1.5,
            BetType::Low => 1.7,
        },
        8 => match bet_type {
            BetType::High => 1.6,
            BetType::Low => 1.6,
        },
        9 => match bet_type {
            BetType::High => 1.8,
            BetType::Low => 1.6,
        },
        10 => match bet_type {
            BetType::High => 2.0,
            BetType::Low => 1.5,
        },
        11 => match bet_type {
            BetType::High => 2.5,
            BetType::Low => 1.4,
        },
        12 => match bet_type {
            BetType::High => 3.0,
            BetType::Low => 1.3,
        },
        13 => match bet_type {
            BetType::High => 4.0,
            BetType::Low => 1.2,
        },
        14 => match bet_type {
            BetType::High => 1.2,
            BetType::Low => 1.2,
        },
        _ => 1.0,
    }
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = admin, space = 8 + State::LEN)]
    pub state: Account<'info, State>,
    #[account(mut)]
    pub admin: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct RequestRandomness<'info> {
    pub switchboard_vrf_program: AccountInfo<'info>,
    #[account(mut)]
    pub vrf_account: AccountInfo<'info>,
    #[account(mut)]
    pub admin: Signer<'info>,
}

#[derive(Accounts)]
pub struct ReceiveRandomness<'info> {
    #[account(mut)]
    pub player: Account<'info, Player>,
    #[account(signer)]
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct StartGame<'info> {
    #[account(mut)]
    pub state: Account<'info, State>,
    #[account(mut)]
    pub player: Account<'info, Player>,
    #[account(signer)]
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct PlaceBet<'info> {
    #[account(mut)]
    pub player: Account<'info, Player>,
    #[account(signer)]
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct FinalizeLeaderboard<'info> {
    #[account(mut)]
    pub state: Account<'info, State>,
    #[account(signer)]
    pub admin: Signer<'info>,
}

#[derive(Accounts)]
pub struct ClaimPrize<'info> {
    #[account(mut)]
    pub state: Account<'info, State>,
    #[account(signer)]
    pub player: Signer<'info>,
    #[account(mut)]
    pub player_wallet: SystemAccount<'info>,
}

#[account]
pub struct State {
    pub admin: Pubkey,
    pub entry_fee: u64,
    pub start_time: i64,
    pub end_time: i64,
    pub leaderboard_size: u8,
    pub leaderboard: Vec<LeaderboardEntry>,
    pub finalized: bool,
    pub finalized_timestamp: i64,
    pub pool: u64,
}

impl State {
    pub const LEN: usize = 32 + 8 + 8 + 8 + 1 + 4 + 8 + 8;
}

#[account]
pub struct Player {
    pub game_id: u64,
    pub start_time: i64,
    pub multiplier: f64,
    pub side_bet_score: i64,
    pub randomness: Option<u64>,
    pub deck: Vec<Card>,
    pub daily_games: u8,
    pub finished: bool,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct Card {
    pub suit: String,
    pub value: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct LeaderboardEntry {
    pub player: Pubkey,
    pub score: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub enum BetType {
    High,
    Low,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub enum SideBetType {
    Color { red: bool },
    Parity { even: bool },
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct BetOutcome {
    pub correct: bool,
    pub multiplier_gain: f64,
    pub side_bet_result: Option<i64>,
}

#[event]
pub struct GameInitialized {
    pub admin: Pubkey,
    pub entry_fee: u64,
    pub start_time: i64,
    pub end_time: i64,
}

#[event]
pub struct RandomnessRequested {
    pub seed: u64,
}

#[event]
pub struct RandomnessReceived {
    pub randomness: u64,
}

#[event]
pub struct GameStarted {
    pub player: Pubkey,
    pub game_id: u64,
}

#[event]
pub struct BetPlaced {
    pub player: Pubkey,
    pub game_id: u64,
    pub bet_type: BetType,
    pub multiplier_gain: f64,
    pub side_bet_result: Option<i64>,
}

#[event]
pub struct GameOver {
    pub player: Pubkey,
    pub game_id: u64,
    pub final_multiplier: f64,
    pub side_bet_score: i64,
}

#[event]
pub struct LeaderboardFinalized {
    pub timestamp: i64,
    pub leaderboard: Vec<LeaderboardEntry>,
}

#[event]
pub struct PrizeClaimed {
    pub player: Pubkey,
    pub position: usize,
    pub prize: u64,
}

#[error_code]
pub enum ErrorCode {
    #[msg("Invalid start time. Start time must be less than end time.")]
    InvalidStartTime,
    #[msg("Entry fee cannot be zero.")]
    InvalidEntryFee,
    #[msg("Daily game limit reached.")]
    DailyLimitReached,
    #[msg("Randomness already received for this game.")]
    RandomnessAlreadyReceived,
    #[msg("Bet placement time expired.")]
    BetTimeExpired,
    #[msg("Game over.")]
    GameOver,
    #[msg("Prize window expired.")]
    PrizeWindowExpired,
    #[msg("Not on leaderboard.")]
    NotOnLeaderboard,
    #[msg("Arithmetic error occurred.")]
    ArithmeticError,
    #[msg("Unauthorized access.")]
    Unauthorized,
}
