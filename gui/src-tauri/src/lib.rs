use std::sync::RwLock;

use clap::Parser;
use anyhow::Result;
use tauri::Manager;
use app_state::{AppState, PDGConfig};
use sigtrail_rs::graphbuilder::LanguageMode;
use graph_building::make_dpdg;
use graph_interaction::{get_n_timeslots, get_partial_graph, toggle_module, set_new_head, reset_head, open_vs_code};

mod argument_parsing;
mod errors;
mod graph_building;
mod app_state;
mod graph_interaction;
mod translation;

#[tauri::command]
fn get_initial_route() -> String {
    "/loading_screen".into()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> Result<()> {
    let args = argument_parsing::Args::parse().validate()?;
    let mut state = AppState::new();

    let language_mode = args.language;
    let top_module = args.top_module;
    // Fill in the VCD scopes based on the source language if unprovided
    let extra_scopes = args.extra_scopes.unwrap_or_else(|| match language_mode {
        LanguageMode::Chisel | LanguageMode::FIR => { vec!["TOP".into(), "svsimTestbench".into(), "dut".into()] }
        LanguageMode::SpinalHDL => { vec!["TOP".into(), top_module.clone()] }
    });

    state.pdg_config = Some(PDGConfig {
        criterion: args.slice_criterion,
        pdg_path: args.pdg_path.into(),
        vcd_path: args.vcd_path.into(),
        hgldd_path: args.hgldd_path.into(),
        top_module,
        extra_scopes,
        max_timesteps: args.max_timesteps,
        data_only: args.data_only.unwrap_or(false),
        group_nodes: args.hier_grouping.unwrap_or(false),
        language_mode
    });

    let config = state.pdg_config.as_ref().unwrap();
    let window_title = format!("SigTrail – {} – {:?} – {:?}", config.top_module, config.criterion, config.language_mode);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(RwLock::new(state))
        .invoke_handler(tauri::generate_handler![get_initial_route, make_dpdg, get_n_timeslots, get_partial_graph, toggle_module, set_new_head, reset_head, open_vs_code])
        .setup(move |app| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_title(&window_title);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
    Ok(())
}
