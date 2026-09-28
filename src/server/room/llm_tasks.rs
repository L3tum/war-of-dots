//! LLM agent and advisor task spawning from `GameRoom::new`, extracted
//! verbatim from `mod.rs`. `spawn_llm_agents` spawns one task per LLM
//! agent pair and returns the spawned handles plus the reason-log
//! receiver; `spawn_advisors` spawns one task per advisor-flag entry
//! (human players when an advisor client exists), appends its handles to
//! the given vec, and returns the advice receiver. Both receivers are
//! consumed by the tick loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::Command;
use crate::llm::actions::LlmAction;
use crate::llm::advisor::Advisor;
use crate::llm::agent::{GameState, LlmAgent};
use crate::llm::clients::LlmClient;
use crate::server::protocol::ServerMessage;

/// Spawn one LLM agent task per agent pair. Returns the spawned handles
/// and the reason-log receiver the tick loop consumes.
#[allow(clippy::type_complexity)]
pub(crate) fn spawn_llm_agents(
    state_broadcasts: &HashMap<String, tokio::sync::broadcast::Sender<GameState>>,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    llm_pairs: Vec<(LlmAgent, LlmClient)>,
) -> (
    Vec<JoinHandle<()>>,
    mpsc::Receiver<(String, String, String)>,
) {
    // --- Spawn LLM agent tasks (one per agent) ---
    let mut spawned_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    // Reason log channel: LLM tasks → main loop → all players
    let (reason_tx, reason_rx) = tokio::sync::mpsc::channel::<(String, String, String)>(64);

    for (mut agent, client) in llm_pairs {
        let player_id = agent.id.clone();
        let agent_name = agent.name.clone();

        let state_rx = match state_broadcasts.get(&player_id) {
            Some(tx) => tx.subscribe(),
            None => continue,
        };

        let cmd_tx_clone = cmd_tx.clone();
        let reason_tx_clone = reason_tx.clone();

        let handle = tokio::spawn(async move {
            let mut state_rx = state_rx;
            let mut latest_state: Option<crate::llm::agent::GameState> = None;

            loop {
                tokio::select! {
                    result = state_rx.recv() => {
                        match result {
                            Ok(state) => {
                                latest_state = Some(state);
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_skipped)) => {
                                // Missed some snapshots — receiver still usable
                                // Just wait for the next one
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep(agent.thinking_delay) => {
                        if let Some(state) = latest_state.take() {
                            match agent.step(&client, &state).await {
                                Ok(actions) => {
                                    // Map each provider-neutral action to the
                                    // server's Command, attaching this agent's
                                    // player id as owner (the llm layer is
                                    // ownership-blind by design).
                                    for action in actions {
                                        let cmd = action_to_command(action, &player_id);
                                        let _ = cmd_tx_clone.send(cmd);
                                    }
                                    // Broadcast recent reason log entries
                                    for entry in agent.reason_log.iter().rev().take(5) {
                                        let _ = reason_tx_clone.send((
                                            agent_name.clone(),
                                            entry.action.clone(),
                                            entry.reason.clone(),
                                        )).await;
                                    }
                                    // Trim reason log to prevent unbounded growth
                                    if agent.reason_log.len() > 100 {
                                        agent.reason_log.drain(..agent.reason_log.len() - 100);
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("LLM agent {} step failed: {}", agent_name, e);
                                }
                            }
                        }
                    }
                }
            }
        });
        spawned_handles.push(handle);
    }

    (spawned_handles, reason_rx)
}

/// Map a provider-neutral LLM action to the server's `Command`, attaching
/// the agent's player id as owner (`owner_id` on unit actions, `owner` on
/// city actions). This is the boundary where the player id enters: the
/// llm layer never sees it.
fn action_to_command(action: LlmAction, player_id: &str) -> Command {
    match action {
        LlmAction::MoveTroop { unit_id, path } => Command::MoveTroop {
            unit_id,
            owner_id: player_id.to_string(),
            path,
        },
        LlmAction::RecruitUnit { city_id, unit_type } => Command::RecruitUnit {
            city_id,
            unit_type,
            owner: player_id.to_string(),
        },
        LlmAction::ToggleHealing { unit_id, enabled } => Command::ToggleHealing {
            unit_id,
            owner_id: player_id.to_string(),
            enabled,
        },
        LlmAction::ToggleResupply { unit_id, enabled } => Command::ToggleResupply {
            unit_id,
            owner_id: player_id.to_string(),
            enabled,
        },
        LlmAction::UpgradeCity { city_id } => Command::UpgradeCity {
            city_id,
            owner: player_id.to_string(),
        },
    }
}

/// Spawn one advisor task per player with an advisor enable flag (human
/// players only, when an advisor client is configured — see
/// `setup::prepare_game`). Appends the spawned handles to
/// `spawned_handles` and returns the advice receiver the tick loop
/// consumes.
pub(crate) fn spawn_advisors(
    spawned_handles: &mut Vec<JoinHandle<()>>,
    state_broadcasts: &HashMap<String, tokio::sync::broadcast::Sender<GameState>>,
    advisor_enabled: &HashMap<String, Arc<AtomicBool>>,
    advisor_client_opt: Option<LlmClient>,
) -> mpsc::Receiver<(String, ServerMessage)> {
    // --- Spawn advisor tasks (one per flagged player) ---
    let advisor_client_opt_spawn = advisor_client_opt.clone();
    let advisor_interval = std::time::Duration::from_secs(60);
    // Advice channel: advisor tasks → main loop → player
    let (advice_tx, advice_rx) = tokio::sync::mpsc::channel::<(String, ServerMessage)>(64);

    for player_id in advisor_enabled.keys() {
        let state_rx = match state_broadcasts.get(player_id) {
            Some(tx) => tx.subscribe(),
            None => continue,
        };

        let advisor_client = advisor_client_opt_spawn.clone();
        let advice_tx_clone = advice_tx.clone();
        let pid = player_id.clone();
        // HIGH #1: each task clones its own shared flag — the tick task
        // toggles this exact `Arc` via Command::ToggleAdvisor, so the
        // toggle now reaches the live task (the old per-tick
        // `advisors.get_mut` mutated a map the tasks never read).
        let enabled_flag = Arc::clone(
            advisor_enabled
                .get(player_id)
                .expect("flag map is keyed by the same player id"),
        );

        let handle = tokio::spawn(async move {
            let mut state_rx = state_rx;
            // HIGH #1: the Advisor is created ONCE per task, outside the
            // loop. The old code built a fresh `Advisor::new()` with
            // `set_enabled(true)` every 60 s iteration — the tick loop's
            // toggle did nothing, and the provider was called every 60 s
            // per human player even when "disabled".
            let mut advisor = Advisor::new();
            loop {
                tokio::time::sleep(advisor_interval).await;

                // The toggle gate FIRST: disabled (the default) means no
                // LLM call at all — no state recv, no provider spend.
                if !enabled_flag.load(Ordering::Relaxed) {
                    continue;
                }
                // Re-read after the gate so a concurrent toggle is
                // respected before the call.
                advisor.set_enabled(enabled_flag.load(Ordering::Relaxed));

                let state = match state_rx.recv().await {
                    Ok(s) => s,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_skipped)) => {
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };

                if let Some(ref client) = advisor_client {
                    if let Some(advice) = advisor.get_advice(client, &state).await {
                        let msg = ServerMessage::Advice { text: advice.text };
                        let _ = advice_tx_clone.send((pid.clone(), msg)).await;
                    }
                }
            }
        });
        spawned_handles.push(handle);
    }

    advice_rx
}
