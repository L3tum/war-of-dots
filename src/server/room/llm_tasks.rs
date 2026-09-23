//! LLM agent and advisor task spawning from `GameRoom::new`, extracted
//! verbatim from `mod.rs`. `spawn_llm_agents` spawns one task per LLM
//! agent pair and returns the spawned handles plus the reason-log
//! receiver; `spawn_advisors` spawns one task per enabled advisor,
//! appends its handles to the given vec, and returns the advice
//! receiver. Both receivers are consumed by the tick loop.

use std::collections::HashMap;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::Command;
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
                                Ok(cmds) => {
                                    for cmd in cmds {
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

/// Spawn one advisor task per enabled advisor. Appends the spawned
/// handles to `spawned_handles` and returns the advice receiver the
/// tick loop consumes.
pub(crate) fn spawn_advisors(
    spawned_handles: &mut Vec<JoinHandle<()>>,
    state_broadcasts: &HashMap<String, tokio::sync::broadcast::Sender<GameState>>,
    advisors: &HashMap<String, Advisor>,
    advisor_client_opt: Option<LlmClient>,
) -> mpsc::Receiver<(String, ServerMessage)> {
    // --- Spawn advisor tasks (one per enabled advisor) ---
    let advisor_client_opt_spawn = advisor_client_opt.clone();
    let advisor_interval = std::time::Duration::from_secs(60);
    // Advice channel: advisor tasks → main loop → player
    let (advice_tx, advice_rx) = tokio::sync::mpsc::channel::<(String, ServerMessage)>(64);

    for player_id in advisors.keys() {
        let state_rx = match state_broadcasts.get(player_id) {
            Some(tx) => tx.subscribe(),
            None => continue,
        };

        let advisor_client = advisor_client_opt_spawn.clone();
        let advice_tx_clone = advice_tx.clone();
        let pid = player_id.clone();

        let handle = tokio::spawn(async move {
            let mut state_rx = state_rx;
            loop {
                tokio::time::sleep(advisor_interval).await;

                let state = match state_rx.recv().await {
                    Ok(s) => s,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_skipped)) => {
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };

                if let Some(ref client) = advisor_client {
                    let mut adv = crate::llm::advisor::Advisor::new();
                    adv.set_enabled(true);
                    if let Some(advice) = adv.get_advice(client, &state).await {
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
