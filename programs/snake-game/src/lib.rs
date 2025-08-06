use anchor_lang::{prelude::*, solana_program};
use anchor_lang::solana_program::clock::Clock;
use solana_program::sysvar::instructions::{load_instruction_at_checked, load_current_index_checked};

declare_id!("47aZBskQcoKBXr4nLn2gy7CjSWDo33PytLaeMET2FfBv");

#[program]
pub mod snake_game {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let contract_state = &mut ctx.accounts.contract_state;
        
        // Initialize with zero commission (Anchor init ensures this is a fresh account)
        contract_state.accumulated_commission = 0;
        
        // Commission vault is now created automatically by Anchor with init attribute
        
        Ok(())
    }

    pub fn create_lobby(ctx: Context<CreateLobby>, bet_amount: u64, lobby_id: String, referrer: Option<Pubkey>) -> Result<()> {
        require!(bet_amount >= MIN_BET_AMOUNT, GameError::BetAmountTooSmall);
        require!(lobby_id.len() <= 64, GameError::LobbyIdTooLong);
        require!(!lobby_id.is_empty(), GameError::LobbyIdTooLong);
        // Validate lobby_id contains only safe ASCII alphanumeric characters and common symbols
        require!(
            lobby_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'), 
            GameError::InvalidLobbyId
        );
        
        let creator_key = ctx.accounts.creator.key();
        
        // Prevent self-referrals
        if let Some(referrer_key) = referrer {
            require!(referrer_key != creator_key, GameError::CannotReferSelf);
        }
        
        let lobby = &mut ctx.accounts.lobby;
        let clock = Clock::get()?;
        
        lobby.id = lobby_id;
        lobby.creator = creator_key;
        lobby.bet_amount = bet_amount;
        lobby.status = LobbyStatus::Waiting;
        lobby.created_at = clock.unix_timestamp;
        lobby.opponent = None;
        lobby.winner = None;
        lobby.referrer = referrer;
        lobby.creator_claimed_draw = None;
        lobby.opponent_claimed_draw = None;
        lobby.commission_taken_draw = false;

        // Vault is now created automatically by Anchor with init attribute

        let cpi_context = CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            anchor_lang::system_program::Transfer {
                from: ctx.accounts.creator.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
            },
        );
        anchor_lang::system_program::transfer(cpi_context, bet_amount)?;
        
        emit!(LobbyCreated {
            lobby_id: lobby.id.clone(),
            creator: lobby.creator,
            bet_amount: lobby.bet_amount,
            timestamp: clock.unix_timestamp,
        });
        
        Ok(())
    }

    pub fn join_lobby(ctx: Context<JoinLobby>) -> Result<()> {
        let lobby = &mut ctx.accounts.lobby;
        let clock = Clock::get()?;
        
        require!(lobby.status == LobbyStatus::Waiting, GameError::LobbyNotAvailable);
        require!(lobby.opponent.is_none(), GameError::LobbyFull);
        require!(ctx.accounts.opponent.key() != lobby.creator, GameError::CannotJoinOwnLobby);
        
        lobby.opponent = Some(ctx.accounts.opponent.key());
        lobby.status = LobbyStatus::InProgress;
        lobby.game_started_at = Some(clock.unix_timestamp);
        
        // Transfer bet from opponent to vault
        let cpi_context = CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            anchor_lang::system_program::Transfer {
                from: ctx.accounts.opponent.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
            },
        );
        anchor_lang::system_program::transfer(cpi_context, lobby.bet_amount)?;
        
        emit!(PlayerJoined {
            lobby_id: lobby.id.clone(),
            opponent: ctx.accounts.opponent.key(),
            timestamp: clock.unix_timestamp,
        });
        
        Ok(())
    }

    pub fn claim_prize(ctx: Context<ClaimPrize>, game_signature: Vec<u8>, nonce: u64) -> Result<()> {
        let lobby = &mut ctx.accounts.lobby;
        let clock = Clock::get()?;
        let winner = ctx.accounts.winner.key();
        
        // Critical: Ensure lobby is in progress and hasn't been claimed yet
        require!(lobby.status == LobbyStatus::InProgress, GameError::GameNotInProgress);
        require!(lobby.winner.is_none(), GameError::PrizeAlreadyClaimed);
        
        // Validate winner is a legitimate participant
        require!(winner == lobby.creator || Some(winner) == lobby.opponent, GameError::InvalidWinner);
        
        
        // Verify the winner account is actually signing this transaction
        require!(ctx.accounts.winner.is_signer, GameError::WinnerMustSign);
        
        // Prevent replay attacks by including nonce in signature
        let message = format!("game:{}:{}:{}", lobby.id, winner.to_string(), nonce);
        let message_bytes = message.as_bytes();
        
        // Extract signature from game_signature (should be 64 bytes)
        require!(game_signature.len() == 64, GameError::InvalidSignature);
        
        let backend_pubkey_bytes = BACKEND_AUTHORITY.to_bytes();
        
        // Verify ed25519 signature using instruction sysvar
        verify_ed25519_signature(
            &ctx.accounts.instruction_sysvar,
            &backend_pubkey_bytes,
            message_bytes,
            &game_signature,
        )?;
        
        // Atomically update lobby state to prevent race conditions
        lobby.winner = Some(winner);
        lobby.status = LobbyStatus::Completed;
        lobby.completed_at = Some(clock.unix_timestamp);
        
        // Calculate total prize pool (2x bet amount)
        let total_pool = lobby.bet_amount.checked_mul(2).ok_or(GameError::ArithmeticOverflow)?;
        
        // Calculate 5% total commission (2.5% for us, 2.5% for referrer if exists)
        let total_commission = total_pool.checked_mul(5).ok_or(GameError::ArithmeticOverflow)?
            .checked_div(100).ok_or(GameError::ArithmeticOverflow)?;
        
        let (our_commission, referrer_commission) = if lobby.referrer.is_some() {
            // If referrer exists, split 5% equally: 2.5% each
            let half_commission = total_commission.checked_div(2).ok_or(GameError::ArithmeticOverflow)?;
            let remainder = total_commission.checked_sub(half_commission.checked_mul(2).ok_or(GameError::ArithmeticOverflow)?).ok_or(GameError::ArithmeticOverflow)?;
            // Give remainder to us (contract) to handle rounding
            (half_commission.checked_add(remainder).ok_or(GameError::ArithmeticOverflow)?, half_commission)
        } else {
            // If no referrer, we get full 5%
            (total_commission, 0)
        };
        
        let prize_after_commission = total_pool.checked_sub(total_commission).ok_or(GameError::ArithmeticOverflow)?;
        
        // Store our commission amount in contract state for tracking
        let contract_state = &mut ctx.accounts.contract_state;
        contract_state.accumulated_commission = contract_state.accumulated_commission.checked_add(our_commission).ok_or(GameError::ArithmeticOverflow)?;
        
        // Validate vault has sufficient balance before transfers (including rent-exempt amount)
        let vault_balance = ctx.accounts.vault.lamports();
        let rent_exempt_amount = Rent::get()?.minimum_balance(0);
        require!(vault_balance >= total_pool + rent_exempt_amount, GameError::InsufficientVaultBalance);
        
        
        // Transfer our commission to commission vault using safe methods
        ctx.accounts.vault.sub_lamports(our_commission)?;
        ctx.accounts.commission_vault.add_lamports(our_commission)?;
        
        // Transfer referrer commission if referrer exists and account provided
        if let Some(referrer_key) = lobby.referrer {
            if let Some(referrer_account) = &ctx.accounts.referrer {
                // Validate referrer account matches the one stored in lobby
                require!(referrer_account.key() == referrer_key, GameError::InvalidReferrer);
                
                // Check if referrer account has sufficient balance to remain rent-exempt after receiving commission
                let current_balance = referrer_account.lamports();
                let rent_exempt_minimum = Rent::get()?.minimum_balance(0);
                
                // Only transfer if referrer account can safely receive funds
                // If account doesn't exist or has insufficient rent, add commission to our vault instead
                if current_balance > 0 || referrer_commission >= rent_exempt_minimum {
                    ctx.accounts.vault.sub_lamports(referrer_commission)?;
                    referrer_account.add_lamports(referrer_commission)?;
                } else {
                    // Add referrer's commission to our commission (safer fallback)
                    contract_state.accumulated_commission = contract_state.accumulated_commission
                        .checked_add(referrer_commission).ok_or(GameError::ArithmeticOverflow)?;
                    ctx.accounts.vault.sub_lamports(referrer_commission)?;
                    ctx.accounts.commission_vault.add_lamports(referrer_commission)?;
                }
            } else {
                // If referrer account not provided, add referrer's commission to our commission
                contract_state.accumulated_commission = contract_state.accumulated_commission
                    .checked_add(referrer_commission).ok_or(GameError::ArithmeticOverflow)?;
                ctx.accounts.vault.sub_lamports(referrer_commission)?;
                ctx.accounts.commission_vault.add_lamports(referrer_commission)?;
            }
        }
        
        // Transfer prize but keep rent-exempt amount in vault
        ctx.accounts.vault.sub_lamports(prize_after_commission)?;
        ctx.accounts.winner.add_lamports(prize_after_commission)?;
        
        // Ensure vault retains rent-exempt status
        let remaining_balance = ctx.accounts.vault.lamports();
        require!(remaining_balance >= rent_exempt_amount, GameError::InsufficientVaultBalance);
        
        emit!(GameCompleted {
            lobby_id: lobby.id.clone(),
            winner,
            prize: prize_after_commission,
            timestamp: clock.unix_timestamp,
        });
        
        Ok(())
    }

    pub fn claim_commission(ctx: Context<ClaimCommission>, amount: u64) -> Result<()> {
        let contract_state = &mut ctx.accounts.contract_state;
        
        require!(contract_state.accumulated_commission >= amount, GameError::InsufficientCommission);
        
        // Validate commission vault has sufficient balance
        let vault_balance = ctx.accounts.commission_vault.lamports();
        require!(vault_balance >= amount, GameError::InsufficientVaultBalance);
        
        contract_state.accumulated_commission = contract_state.accumulated_commission.checked_sub(amount).ok_or(GameError::ArithmeticOverflow)?;
        
        // Transfer from commission vault to claimer using safe methods
        // Keep rent-exempt amount in commission vault
        let rent_exempt_amount = Rent::get()?.minimum_balance(0);
        let remaining_balance = vault_balance.checked_sub(amount).ok_or(GameError::ArithmeticOverflow)?;
        require!(remaining_balance >= rent_exempt_amount, GameError::InsufficientVaultBalance);
        
        ctx.accounts.commission_vault.sub_lamports(amount)?;
        ctx.accounts.commission_claimer.add_lamports(amount)?;
        
        emit!(CommissionClaimed {
            claimer: ctx.accounts.commission_claimer.key(),
            amount,
            timestamp: Clock::get()?.unix_timestamp,
        });
        
        Ok(())
    }


    pub fn claim_draw_refund(ctx: Context<ClaimDrawRefund>, game_signature: Vec<u8>, nonce: u64) -> Result<()> {
        let lobby = &mut ctx.accounts.lobby;
        let clock = Clock::get()?;
        let claimer = ctx.accounts.claimer.key();
        
        // Ensure lobby is in progress or already marked as draw
        require!(
            lobby.status == LobbyStatus::InProgress || lobby.status == LobbyStatus::Draw, 
            GameError::GameNotInProgress
        );
        
        // Validate claimer is a legitimate participant
        require!(claimer == lobby.creator || Some(claimer) == lobby.opponent, GameError::InvalidClaimer);
        
        // Ensure both participants exist (can't have draw without opponent)
        require!(lobby.opponent.is_some(), GameError::OpponentNotFound);
        
        // Check if claimer has already claimed their refund
        if claimer == lobby.creator {
            require!(lobby.creator_claimed_draw.is_none(), GameError::RefundAlreadyClaimed);
        } else {
            require!(lobby.opponent_claimed_draw.is_none(), GameError::RefundAlreadyClaimed);
        }
        
        // Verify the claimer account is actually signing this transaction
        require!(ctx.accounts.claimer.is_signer, GameError::ClaimerMustSign);
        
        // Prevent replay attacks by including nonce in signature
        let message = format!("draw:{}:{}:{}", lobby.id, claimer.to_string(), nonce);
        let message_bytes = message.as_bytes();
        
        // Extract signature from game_signature (should be 64 bytes)
        require!(game_signature.len() == 64, GameError::InvalidSignature);
        
        let backend_pubkey_bytes = BACKEND_AUTHORITY.to_bytes();
        
        // Verify ed25519 signature using instruction sysvar
        verify_ed25519_signature(
            &ctx.accounts.instruction_sysvar,
            &backend_pubkey_bytes,
            message_bytes,
            &game_signature,
        )?;
        
        // Calculate total prize pool (2x bet amount)
        let total_pool = lobby.bet_amount.checked_mul(2).ok_or(GameError::ArithmeticOverflow)?;
        
        // Calculate 5% total commission (2.5% for us, 2.5% for referrer if exists)
        let total_commission = total_pool.checked_mul(5).ok_or(GameError::ArithmeticOverflow)?
            .checked_div(100).ok_or(GameError::ArithmeticOverflow)?;

        let (our_commission, referrer_commission, commission_per_player) = 
            if !lobby.commission_taken_draw {
                // Commission not taken yet, calculate and take it
                // Calculate remainder to handle odd total_commission correctly
                let remainder = total_commission.checked_rem(2).ok_or(GameError::ArithmeticOverflow)?;
                let half_commission = total_commission.checked_div(2).ok_or(GameError::ArithmeticOverflow)?;
                
                let (our_comm, ref_comm) = if lobby.referrer.is_some() {
                    // If referrer exists, split 5% equally: 2.5% each
                    // Don't add remainder to our_commission to avoid rent-exempt issues
                    (half_commission, half_commission)
                } else {
                    // If no referrer, we get total commission minus remainder to keep vault rent-exempt
                    (total_commission.checked_sub(remainder).ok_or(GameError::ArithmeticOverflow)?, 0)
                };
                
                // Calculate commission per player using ceiling division to include remainder
                let commission_per_player = total_commission.checked_add(1).ok_or(GameError::ArithmeticOverflow)?
                    .checked_div(2).ok_or(GameError::ArithmeticOverflow)?;
                (our_comm, ref_comm, commission_per_player)
            } else {
                // Commission already taken, no commission for this call
                // Use ceiling division to match the original calculation
                let commission_per_player = total_commission.checked_add(1).ok_or(GameError::ArithmeticOverflow)?
                    .checked_div(2).ok_or(GameError::ArithmeticOverflow)?;
                (0, 0, commission_per_player)
            };
        
        let refund_amount = lobby.bet_amount.checked_sub(commission_per_player).ok_or(GameError::ArithmeticOverflow)?;
        
        // Handle commission transfers only if commission hasn't been taken yet
        if !lobby.commission_taken_draw {
            // Store our commission amount in contract state for tracking
            let contract_state = &mut ctx.accounts.contract_state;
            contract_state.accumulated_commission = contract_state.accumulated_commission.checked_add(our_commission).ok_or(GameError::ArithmeticOverflow)?;
            
            // Transfer our commission to commission vault
            ctx.accounts.vault.sub_lamports(our_commission)?;
            ctx.accounts.commission_vault.add_lamports(our_commission)?;
            
            // Transfer referrer commission if referrer exists and account provided
            if let Some(referrer_key) = lobby.referrer {
                if let Some(referrer_account) = &ctx.accounts.referrer {
                    // Validate referrer account matches the one stored in lobby
                    require!(referrer_account.key() == referrer_key, GameError::InvalidReferrer);
                    
                    // Check if referrer account has sufficient balance to remain rent-exempt after receiving commission
                    let current_balance = referrer_account.lamports();
                    let rent_exempt_minimum = Rent::get()?.minimum_balance(0);
                    
                    // Only transfer if referrer account can safely receive funds
                    // If account doesn't exist or has insufficient rent, add commission to our vault instead
                    if current_balance > 0 || referrer_commission >= rent_exempt_minimum {
                        ctx.accounts.vault.sub_lamports(referrer_commission)?;
                        referrer_account.add_lamports(referrer_commission)?;
                    } else {
                        // Add referrer's commission to our commission (safer fallback)
                        contract_state.accumulated_commission = contract_state.accumulated_commission
                            .checked_add(referrer_commission).ok_or(GameError::ArithmeticOverflow)?;
                        ctx.accounts.vault.sub_lamports(referrer_commission)?;
                        ctx.accounts.commission_vault.add_lamports(referrer_commission)?;
                    }
                } else {
                    // If referrer account not provided, add referrer's commission to our commission
                    contract_state.accumulated_commission = contract_state.accumulated_commission
                        .checked_add(referrer_commission).ok_or(GameError::ArithmeticOverflow)?;
                    ctx.accounts.vault.sub_lamports(referrer_commission)?;
                    ctx.accounts.commission_vault.add_lamports(referrer_commission)?;
                }
            }
            
            // Mark commission as taken
            lobby.commission_taken_draw = true;
        }
        
        // Validate vault has sufficient balance before transfers (including rent-exempt amount)
        let vault_balance = ctx.accounts.vault.lamports();
        let rent_exempt_amount = Rent::get()?.minimum_balance(0);
        require!(vault_balance >= refund_amount + rent_exempt_amount, GameError::InsufficientVaultBalance);
        
        // Transfer refund to claimer
        ctx.accounts.vault.sub_lamports(refund_amount)?;
        ctx.accounts.claimer.add_lamports(refund_amount)?;
        
        // Mark this participant as having claimed their refund and set status to Draw if needed
        if claimer == lobby.creator {
            lobby.creator_claimed_draw = Some(true);
        } else {
            lobby.opponent_claimed_draw = Some(true);
        }
        
        // Set lobby status to Draw and completion time if this is the first claim
        if lobby.status == LobbyStatus::InProgress {
            lobby.status = LobbyStatus::Draw;
            lobby.completed_at = Some(clock.unix_timestamp);
            
            emit!(GameDeclaredDraw {
                lobby_id: lobby.id.clone(),
                timestamp: clock.unix_timestamp,
            });
        }
        
        // Ensure vault retains rent-exempt status
        let remaining_balance = ctx.accounts.vault.lamports();
        require!(remaining_balance >= rent_exempt_amount, GameError::InsufficientVaultBalance);
        
        emit!(DrawRefundClaimed {
            lobby_id: lobby.id.clone(),
            claimer,
            refund_amount,
            timestamp: clock.unix_timestamp,
        });
        
        Ok(())
    }


    pub fn cancel_game_timeout(ctx: Context<CancelGameTimeout>) -> Result<()> {
        let lobby = &mut ctx.accounts.lobby;
        let clock = Clock::get()?;
        let canceller = ctx.accounts.canceller.key();
        
        // Validate canceller is a participant in the game
        require!(
            canceller == lobby.creator || Some(canceller) == lobby.opponent,
            GameError::OnlyParticipantsCanCancel
        );
        
        // Check timeout conditions based on lobby status
        match lobby.status {
            LobbyStatus::Waiting => {
                // 60 minutes timeout from lobby creation
                let timeout_threshold = lobby.created_at.checked_add(GAME_TIMEOUT_SECONDS)
                    .ok_or(GameError::ArithmeticOverflow)?;
                require!(
                    clock.unix_timestamp >= timeout_threshold,
                    GameError::TimeoutNotReached
                );
                
                // Validate creator account matches lobby creator
                require!(ctx.accounts.creator.key() == lobby.creator, GameError::InvalidCreator);
                
                // Refund only creator's bet (opponent hasn't joined yet)
                let vault_balance = ctx.accounts.vault.lamports();
                let rent_exempt_amount = Rent::get()?.minimum_balance(0);
                let refund_amount = lobby.bet_amount;
                
                require!(vault_balance >= refund_amount + rent_exempt_amount, GameError::InsufficientVaultBalance);
                
                ctx.accounts.vault.sub_lamports(refund_amount)?;
                ctx.accounts.creator.add_lamports(refund_amount)?;
            },
            LobbyStatus::InProgress => {
                // 60 minutes timeout from game start
                let game_start = lobby.game_started_at.ok_or(GameError::GameNotStarted)?;
                let timeout_threshold = game_start.checked_add(GAME_TIMEOUT_SECONDS)
                    .ok_or(GameError::ArithmeticOverflow)?;
                require!(
                    clock.unix_timestamp >= timeout_threshold,
                    GameError::TimeoutNotReached
                );
                
                // Calculate total prize pool (2x bet amount)
                let total_pool = lobby.bet_amount.checked_mul(2).ok_or(GameError::ArithmeticOverflow)?;
                
                // Calculate 5% total commission (2.5% for us, 2.5% for referrer if exists)
                let total_commission = total_pool.checked_mul(5).ok_or(GameError::ArithmeticOverflow)?
                    .checked_div(100).ok_or(GameError::ArithmeticOverflow)?;
                
                // Calculate remainder to handle odd total_commission correctly
                let remainder = total_commission.checked_rem(2).ok_or(GameError::ArithmeticOverflow)?;
                let half_commission = total_commission.checked_div(2).ok_or(GameError::ArithmeticOverflow)?;

                let (our_commission, referrer_commission) = if lobby.referrer.is_some() {
                    // If referrer exists, split 5% equally: 2.5% each
                    // Don't add remainder to our_commission to avoid rent-exempt issues
                    (half_commission, half_commission)
                } else {
                    // If no referrer, we get total commission minus remainder to keep vault rent-exempt
                    (total_commission.checked_sub(remainder).ok_or(GameError::ArithmeticOverflow)?, 0)
                };
                
                // Calculate commission per player using ceiling division to include remainder
                let commission_per_player = total_commission.checked_add(1).ok_or(GameError::ArithmeticOverflow)?
                    .checked_div(2).ok_or(GameError::ArithmeticOverflow)?;
                let refund_per_player = lobby.bet_amount.checked_sub(commission_per_player).ok_or(GameError::ArithmeticOverflow)?;
                
                let vault_balance = ctx.accounts.vault.lamports();
                let rent_exempt_amount = Rent::get()?.minimum_balance(0);
                
                require!(vault_balance >= total_pool + rent_exempt_amount, GameError::InsufficientVaultBalance);
                
                // Store our commission amount in contract state for tracking
                let contract_state = &mut ctx.accounts.contract_state;
                contract_state.accumulated_commission = contract_state.accumulated_commission.checked_add(our_commission).ok_or(GameError::ArithmeticOverflow)?;
                
                // Transfer our commission to commission vault
                ctx.accounts.vault.sub_lamports(our_commission)?;
                ctx.accounts.commission_vault.add_lamports(our_commission)?;
                
                // Transfer referrer commission if referrer exists and account provided
                if let Some(referrer_key) = lobby.referrer {
                    if let Some(referrer_account) = &ctx.accounts.referrer {
                        // Validate referrer account matches the one stored in lobby
                        require!(referrer_account.key() == referrer_key, GameError::InvalidReferrer);
                        
                        // Check if referrer account has sufficient balance to remain rent-exempt after receiving commission
                        let current_balance = referrer_account.lamports();
                        let rent_exempt_minimum = Rent::get()?.minimum_balance(0);
                        
                        // Only transfer if referrer account can safely receive funds
                        // If account doesn't exist or has insufficient rent, add commission to our vault instead
                        if current_balance > 0 || referrer_commission >= rent_exempt_minimum {
                            ctx.accounts.vault.sub_lamports(referrer_commission)?;
                            referrer_account.add_lamports(referrer_commission)?;
                        } else {
                            // Add referrer's commission to our commission (safer fallback)
                            contract_state.accumulated_commission = contract_state.accumulated_commission
                                .checked_add(referrer_commission).ok_or(GameError::ArithmeticOverflow)?;
                            ctx.accounts.vault.sub_lamports(referrer_commission)?;
                            ctx.accounts.commission_vault.add_lamports(referrer_commission)?;
                        }
                    } else {
                        // If referrer account not provided, add referrer's commission to our commission
                        contract_state.accumulated_commission = contract_state.accumulated_commission
                            .checked_add(referrer_commission).ok_or(GameError::ArithmeticOverflow)?;
                        ctx.accounts.vault.sub_lamports(referrer_commission)?;
                        ctx.accounts.commission_vault.add_lamports(referrer_commission)?;
                    }
                }
                
                // Validate creator account matches lobby creator
                require!(ctx.accounts.creator.key() == lobby.creator, GameError::InvalidCreator);
                
                // Refund creator (minus commission)
                ctx.accounts.vault.sub_lamports(refund_per_player)?;
                ctx.accounts.creator.add_lamports(refund_per_player)?;
                
                // Validate and refund opponent (minus commission)
                let opponent_key = lobby.opponent.ok_or(GameError::OpponentNotFound)?;
                require!(ctx.accounts.opponent.key() == opponent_key, GameError::InvalidOpponent);
                
                ctx.accounts.vault.sub_lamports(refund_per_player)?;
                ctx.accounts.opponent.add_lamports(refund_per_player)?;
            },
            LobbyStatus::Completed => {
                return Err(GameError::GameAlreadyCompleted.into());
            },
            LobbyStatus::Cancelled => {
                return Err(GameError::GameAlreadyCancelled.into());
            },
            LobbyStatus::Draw => {
                return Err(GameError::GameAlreadyCompleted.into());
            }
        }
        
        lobby.status = LobbyStatus::Cancelled;
        
        emit!(GameTimeoutCancelled {
            lobby_id: lobby.id.clone(),
            canceller,
            timestamp: clock.unix_timestamp,
        });
        
        Ok(())
    }

    pub fn close_lobby(ctx: Context<CloseLobby>) -> Result<()> {
        let lobby = &ctx.accounts.lobby;
        
        // Ensure game is completely finished
        require!(
            matches!(
                lobby.status,
                LobbyStatus::Completed | LobbyStatus::Cancelled | LobbyStatus::Draw
            ),
            GameError::GameNotFinished
        );
        
        // Ensure vault only contains rent-exempt minimum
        let rent_min = Rent::get()?.minimum_balance(0);
        require!(
            ctx.accounts.vault.lamports() == rent_min,
            GameError::VaultNotEmpty
        );
        
        // Close vault manually since we can't use close attribute on AccountInfo
        let vault_balance = ctx.accounts.vault.lamports();
        ctx.accounts.vault.sub_lamports(vault_balance)?;
        ctx.accounts.creator.add_lamports(vault_balance)?;
        
        // Lobby account will be closed automatically by the close attribute
        
        Ok(())
    }
}

// Ed25519 signature verification helper function
fn verify_ed25519_signature(
    instruction_sysvar: &AccountInfo,
    expected_public_key: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<()> {
    let current_index = load_current_index_checked(instruction_sysvar)?;
    require!(current_index > 0, GameError::InvalidSignature);

    // Safe conversion: current_index is guaranteed > 0 due to check above
    let ed25519_instruction = load_instruction_at_checked(
        current_index.saturating_sub(1) as usize, 
        instruction_sysvar
    )?;
    
    // Verify this is actually an Ed25519 instruction
    require!(ed25519_instruction.program_id == solana_program::ed25519_program::ID, GameError::InvalidSignature);
    
    // Verify the content of the Ed25519 instruction
    let instruction_data = ed25519_instruction.data;
    
    // Check full structure size: 2 bytes header + 14 bytes Ed25519SignatureOffsets
    const FULL_HEADER_SIZE: usize = SIGNATURE_OFFSETS_START + SIGNATURE_OFFSETS_SERIALIZED_SIZE;
    require!(instruction_data.len() >= FULL_HEADER_SIZE, GameError::InvalidSignature);

    let num_signatures = instruction_data[0];
    require!(num_signatures == 1, GameError::InvalidSignature);
    // instruction_data[1] is padding byte, ignore

    // Parse COMPLETE Ed25519SignatureOffsets structure (14 bytes starting from index 2)
    let signature_offset = u16::from_le_bytes([instruction_data[2], instruction_data[3]]);
    let signature_instruction_index = u16::from_le_bytes([instruction_data[4], instruction_data[5]]);
    let public_key_offset = u16::from_le_bytes([instruction_data[6], instruction_data[7]]);
    let public_key_instruction_index = u16::from_le_bytes([instruction_data[8], instruction_data[9]]);
    let message_data_offset = u16::from_le_bytes([instruction_data[10], instruction_data[11]]);
    let message_data_size = u16::from_le_bytes([instruction_data[12], instruction_data[13]]);
    let message_instruction_index = u16::from_le_bytes([instruction_data[14], instruction_data[15]]);

    require!(signature_instruction_index == u16::MAX, GameError::InvalidSignature);
    require!(public_key_instruction_index == u16::MAX, GameError::InvalidSignature);
    require!(message_instruction_index == u16::MAX, GameError::InvalidSignature);

    let data_start = FULL_HEADER_SIZE;

    // Verify public key with safe bounds checking
    let pubkey_start = public_key_offset as usize;
    let pubkey_end = pubkey_start.checked_add(PUBKEY_SERIALIZED_SIZE).ok_or(GameError::InvalidSignature)?;
    require!(pubkey_start >= data_start, GameError::InvalidSignature);
    require!(pubkey_end <= instruction_data.len(), GameError::InvalidSignature);
    require!(&instruction_data[pubkey_start..pubkey_end] == expected_public_key, GameError::InvalidSignature);

    // Verify message with safe bounds checking
    let msg_start = message_data_offset as usize;
    let msg_size = message_data_size as usize;
    let msg_end = msg_start.checked_add(msg_size).ok_or(GameError::InvalidSignature)?;
    require!(msg_start >= data_start, GameError::InvalidSignature);
    require!(msg_end <= instruction_data.len(), GameError::InvalidSignature);
    require!(&instruction_data[msg_start..msg_end] == message, GameError::InvalidSignature);

    // Verify signature with safe bounds checking
    let sig_start = signature_offset as usize;
    let sig_end = sig_start.checked_add(SIGNATURE_SERIALIZED_SIZE).ok_or(GameError::InvalidSignature)?;
    require!(sig_start >= data_start, GameError::InvalidSignature);
    require!(sig_end <= instruction_data.len(), GameError::InvalidSignature);
    require!(&instruction_data[sig_start..sig_end] == signature, GameError::InvalidSignature);

    // If we reach here, Ed25519 precompile has successfully verified the signature
    Ok(())
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + ContractState::INIT_SPACE,
        seeds = [b"contract_state"],
        bump
    )]
    pub contract_state: Account<'info, ContractState>,
    
    #[account(
        init,
        payer = authority,
        space = 0,
        seeds = [b"commission_vault"],
        bump
    )]
    /// CHECK: Commission vault PDA for storing commission funds
    pub commission_vault: AccountInfo<'info>,
    
    #[account(
        mut,
        constraint = authority.key() == BACKEND_AUTHORITY
    )]
    pub authority: Signer<'info>,
    
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(bet_amount: u64, lobby_id: String, referrer: Option<Pubkey>)]
pub struct CreateLobby<'info> {
    #[account(
        init,
        payer = creator,
        space = 8 + Lobby::INIT_SPACE,
        seeds = [b"lobby", lobby_id.as_bytes()],
        bump
    )]
    pub lobby: Account<'info, Lobby>,
    
    #[account(
        init,
        payer = creator,
        space = 0,
        seeds = [b"vault", lobby.key().as_ref()],
        bump
    )]
    /// CHECK: Vault PDA for storing bet funds
    pub vault: AccountInfo<'info>,
    
    #[account(mut)]
    pub creator: Signer<'info>,
    
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct JoinLobby<'info> {
    #[account(mut)]
    pub lobby: Account<'info, Lobby>,
    
    #[account(
        mut,
        seeds = [b"vault", lobby.key().as_ref()],
        bump
    )]
    /// CHECK: This is just a vault account
    pub vault: AccountInfo<'info>,
    
    #[account(mut)]
    pub opponent: Signer<'info>,
    
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClaimPrize<'info> {
    #[account(mut)]
    pub lobby: Account<'info, Lobby>,
    
    #[account(
        mut,
        seeds = [b"vault", lobby.key().as_ref()],
        bump,
        owner = crate::ID
    )]
    /// CHECK: This is just a vault account
    pub vault: AccountInfo<'info>,
    
    #[account(
        mut,
        seeds = [b"commission_vault"],
        bump,
        owner = crate::ID
    )]
    /// CHECK: This is the global commission vault
    pub commission_vault: AccountInfo<'info>,
    
    /// The winner who is claiming the prize - must be a signer
    #[account(mut)]
    pub winner: Signer<'info>,
    
    /// CHECK: Optional referrer account to receive commission
    pub referrer: Option<AccountInfo<'info>>,
    
    #[account(
        mut,
        seeds = [b"contract_state"],
        bump
    )]
    pub contract_state: Account<'info, ContractState>,
    
    /// CHECK: This is the instruction sysvar
    #[account(address = solana_program::sysvar::instructions::ID)]
    pub instruction_sysvar: AccountInfo<'info>,
    
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClaimCommission<'info> {
    #[account(
        mut,
        seeds = [b"contract_state"],
        bump
    )]
    pub contract_state: Account<'info, ContractState>,
    
    #[account(
        mut,
        seeds = [b"commission_vault"],
        bump,
        owner = crate::ID
    )]
    /// CHECK: This is the global commission vault
    pub commission_vault: AccountInfo<'info>,
    
    #[account(
        mut,
        constraint = commission_claimer.key() == COMMISSION_CLAIMER
    )]
    pub commission_claimer: Signer<'info>,
    
    pub system_program: Program<'info, System>,
}


#[derive(Accounts)]
pub struct CancelGameTimeout<'info> {
    #[account(mut)]
    pub lobby: Account<'info, Lobby>,
    
    #[account(
        mut,
        seeds = [b"vault", lobby.key().as_ref()],
        bump,
        owner = crate::ID
    )]
    /// CHECK: This is just a vault account
    pub vault: AccountInfo<'info>,
    
    #[account(
        mut,
        seeds = [b"commission_vault"],
        bump,
        owner = crate::ID
    )]
    /// CHECK: This is the global commission vault
    pub commission_vault: AccountInfo<'info>,
    
    /// CHECK: Creator account to receive refund (validated against lobby.creator)
    #[account(mut)]
    pub creator: AccountInfo<'info>,
    
    /// CHECK: Opponent account to receive refund (validated against lobby.opponent)
    #[account(mut)]
    pub opponent: AccountInfo<'info>,
    
    /// The participant (creator or opponent) who is cancelling the game
    #[account(mut)]
    pub canceller: Signer<'info>,
    
    /// CHECK: Optional referrer account to receive commission
    pub referrer: Option<AccountInfo<'info>>,
    
    #[account(
        mut,
        seeds = [b"contract_state"],
        bump
    )]
    pub contract_state: Account<'info, ContractState>,
    
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClaimDrawRefund<'info> {
    #[account(mut)]
    pub lobby: Account<'info, Lobby>,
    
    #[account(
        mut,
        seeds = [b"vault", lobby.key().as_ref()],
        bump,
        owner = crate::ID
    )]
    /// CHECK: This is just a vault account
    pub vault: AccountInfo<'info>,
    
    #[account(
        mut,
        seeds = [b"commission_vault"],
        bump,
        owner = crate::ID
    )]
    /// CHECK: This is the global commission vault
    pub commission_vault: AccountInfo<'info>,
    
    /// The participant claiming their refund - must be a signer
    #[account(mut)]
    pub claimer: Signer<'info>,
    
    /// CHECK: Optional referrer account to receive commission
    pub referrer: Option<AccountInfo<'info>>,
    
    #[account(
        mut,
        seeds = [b"contract_state"],
        bump
    )]
    pub contract_state: Account<'info, ContractState>,
    
    /// CHECK: This is the instruction sysvar
    #[account(address = solana_program::sysvar::instructions::ID)]
    pub instruction_sysvar: AccountInfo<'info>,
    
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CloseLobby<'info> {
    #[account(
        mut,
        close = creator,
        has_one = creator
    )]
    pub lobby: Account<'info, Lobby>,
    
    #[account(
        mut,
        seeds = [b"vault", lobby.key().as_ref()],
        bump,
        owner = crate::ID
    )]
    /// CHECK: This is just a vault account
    pub vault: AccountInfo<'info>,
    
    #[account(mut)]
    pub creator: Signer<'info>,
    
    pub system_program: Program<'info, System>,
}

#[account]
#[derive(InitSpace)]
pub struct Lobby {
    #[max_len(64)]
    pub id: String,
    pub creator: Pubkey,
    pub opponent: Option<Pubkey>,
    pub bet_amount: u64,
    pub status: LobbyStatus,
    pub winner: Option<Pubkey>,
    pub referrer: Option<Pubkey>,
    pub creator_claimed_draw: Option<bool>,
    pub opponent_claimed_draw: Option<bool>,
    pub commission_taken_draw: bool,
    pub created_at: i64,
    pub game_started_at: Option<i64>,
    pub completed_at: Option<i64>,
}

#[account]
#[derive(InitSpace)]
pub struct ContractState {
    pub accumulated_commission: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq, InitSpace)]
pub enum LobbyStatus {
    Waiting,
    InProgress,
    Completed,
    Cancelled,
    Draw,
}

#[error_code]
pub enum GameError {
    #[msg("Lobby is not available")]
    LobbyNotAvailable,
    #[msg("Lobby is full")]
    LobbyFull,
    #[msg("Cannot join your own lobby")]
    CannotJoinOwnLobby,
    #[msg("Game is not in progress")]
    GameNotInProgress,
    #[msg("Invalid winner")]
    InvalidWinner,
    #[msg("Invalid game signature")]
    InvalidSignature,
    #[msg("Insufficient commission available")]
    InsufficientCommission,
    #[msg("Invalid creator provided")]
    InvalidCreator,
    #[msg("Insufficient vault balance")]
    InsufficientVaultBalance,
    #[msg("Invalid vault owner")]
    InvalidVaultOwner,
    #[msg("Bet amount is too small, minimum is 0.01 SOL")]
    BetAmountTooSmall,
    #[msg("Arithmetic overflow occurred")]
    ArithmeticOverflow,
    #[msg("Prize has already been claimed")]
    PrizeAlreadyClaimed,
    #[msg("Winner must sign the transaction")]
    WinnerMustSign,
    #[msg("Only participants can cancel the game")]
    OnlyParticipantsCanCancel,
    #[msg("Timeout period has not been reached")]
    TimeoutNotReached,
    #[msg("Game has not started yet")]
    GameNotStarted,
    #[msg("Game already completed")]
    GameAlreadyCompleted,
    #[msg("Game already cancelled")]
    GameAlreadyCancelled,
    #[msg("Opponent not found")]
    OpponentNotFound,
    #[msg("Invalid opponent provided")]
    InvalidOpponent,
    #[msg("Cannot refer yourself")]
    CannotReferSelf,
    #[msg("Invalid referrer provided")]
    InvalidReferrer,
    #[msg("Game is not in draw state")]
    GameNotInDraw,
    #[msg("Invalid claimer")]
    InvalidClaimer,
    #[msg("Claimer must sign the transaction")]
    ClaimerMustSign,
    #[msg("Refund already claimed")]
    RefundAlreadyClaimed,
    #[msg("Game is not finished yet")]
    GameNotFinished,
    #[msg("Vault account is not empty")]
    VaultNotEmpty,
    #[msg("Lobby ID too long, maximum 64 bytes allowed")]
    LobbyIdTooLong,
    #[msg("Lobby ID contains invalid characters, only alphanumeric, underscore, and dash allowed")]
    InvalidLobbyId,
}

// Events
#[event]
pub struct LobbyCreated {
    pub lobby_id: String,
    pub creator: Pubkey,
    pub bet_amount: u64,
    pub timestamp: i64,
}

#[event]
pub struct PlayerJoined {
    pub lobby_id: String,
    pub opponent: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct GameCompleted {
    pub lobby_id: String,
    pub winner: Pubkey,
    pub prize: u64,
    pub timestamp: i64,
}


#[event]
pub struct CommissionClaimed {
    pub claimer: Pubkey,
    pub amount: u64,
    pub timestamp: i64,
}

#[event]
pub struct GameTimeoutCancelled {
    pub lobby_id: String,
    pub canceller: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct GameDeclaredDraw {
    pub lobby_id: String,
    pub timestamp: i64,
}

#[event]
pub struct DrawRefundClaimed {
    pub lobby_id: String,
    pub claimer: Pubkey,
    pub refund_amount: u64,
    pub timestamp: i64,
}

// Backend authority pubkey (replace with your backend's keypair pubkey)
pub const BACKEND_AUTHORITY: Pubkey = solana_program::pubkey!("FrmyQzmFNBeEiUUA1nkv4Yh9KDB8fheeaCQqQZZCp53S");

// Commission claimer pubkey
pub const COMMISSION_CLAIMER: Pubkey = solana_program::pubkey!("3wSMiq3LLjawSCnMpcSrAF7a5D9CazWyLotEaEP4Mkch");

// Minimum bet amount (0.01 SOL = 10_000_000 lamports)
pub const MIN_BET_AMOUNT: u64 = 10_000_000;

// Timeout period for game cancellation (60 minutes in seconds)
pub const GAME_TIMEOUT_SECONDS: i64 = 60 * 60;

// Ed25519 signature verification constants
pub const PUBKEY_SERIALIZED_SIZE: usize = 32;
pub const SIGNATURE_SERIALIZED_SIZE: usize = 64;
pub const SIGNATURE_OFFSETS_SERIALIZED_SIZE: usize = 14;
pub const SIGNATURE_OFFSETS_START: usize = 2;