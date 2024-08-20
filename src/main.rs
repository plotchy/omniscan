use clap::{Parser, ValueHint};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::signal;
use tokio::sync::{oneshot, Semaphore};
use sysinfo::System;
mod data_structures;
mod utils;

use data_structures::{ExitType, FiestaMetadata, ResultMessage, ResultsWriter, SourceType};
use utils::{
    analyze_with_pyrometer, minimize_with_pyrometer, check_child_exit, collect_fiesta_metadatas,
    display_result_distribution, get_num_contracts, get_output_path, get_timeouts,
};

use crate::utils::{build_pyrometer, get_path_str_for_result_message};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Path to the Pyrometer project's Cargo.toml. ie: `../pyrometer/Cargo.toml`
    #[clap(long, value_name = "PYROMETER_PATH")]
    pub pyrometer_manifest_path: PathBuf,

    /// Path to the smart-contract-fiesta root directory
    #[clap(value_hint = ValueHint::FilePath, value_name = "PATH")]
    pub path: String,

    /// The number of contracts to run pyrometer on. Default is 1000.
    /// If set to 0, all contracts will be analyzed
    #[clap(long, short, alias = "num-contracts")]
    pub num: Option<usize>,

    /// Timeout for each pyrometer process in secs. Default is 1 second. Decimals supported.
    /// If set to 0, there will be no timeout. (Not advised)
    #[clap(long, short)]
    pub timeout: Option<f64>,

    /// Where to save the results file, default is "./data/results_MM-DD_HH-MM.csv"
    #[clap(long, short)]
    pub output: Option<String>,

    /// The number of concurrent processes to use for the analysis. Default is the number of cores
    #[clap(long, short)]
    pub jobs: Option<u8>,

    /// The number of contracts to initially skip over. Default is 0.
    /// This is intended for debugging purposes
    #[clap(long, short)]
    pub skip_contracts: Option<usize>,

    /// Output path for minimized contracts. Recommended is `./tmp`
    #[clap(long, value_name = "MINIMIZE_PATH")]
    pub minimize_path: Option<String>,
}

fn main() {
    let args = Args::parse();
    let pyrometer_bin = build_pyrometer(&args.pyrometer_manifest_path);
    let abs_fiesta_path = std::path::PathBuf::from(args.path.clone());

    if !abs_fiesta_path.exists() && !abs_fiesta_path.is_dir() {
        eprintln!("The path {} does not exist or is not a dir", args.path);
        std::process::exit(1);
    }

    let output_path = get_output_path(args.output);
    let jobs = args.jobs.unwrap_or_else(|| num_cpus::get() as u8);
    let (pyrometer_timeout, rx_loop_timeout) = get_timeouts(args.timeout);
    let num_contracts = get_num_contracts(args.num);
    let skip_contracts = args.skip_contracts.unwrap_or(0);

    let fiesta_metadatas =
        collect_fiesta_metadatas(&abs_fiesta_path, num_contracts, skip_contracts);

    let total_contracts = fiesta_metadatas.len();
    println!("📚 Beginning analysis of {} contracts with {:.1}s timeout", total_contracts, pyrometer_timeout);
    println!("💡 Press Ctrl-C to exit analysis early anytime");

    let early_exit = Arc::new(AtomicBool::new(false));
    let early_exit_clone_rx = early_exit.clone();
    let early_exit_clone_tx = early_exit.clone();

    let (tx, rx) = mpsc::channel();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();

    let runtime = Runtime::new().unwrap();
    let handle = runtime.handle().clone();

    // Spawn the early-exit handler task
    handle.spawn(async move {
        match signal::ctrl_c().await {
            Ok(()) => {
                early_exit.store(true, Ordering::Relaxed);
            }
            Err(err) => {
                eprintln!("Unable to listen for shutdown signal: {}", err);
            }
        }
    });

    // Spawn tx_loop and rx_loop
    let rx_handle = handle.spawn(async move {
        rx_loop(
            rx,
            stop_rx,
            output_path,
            rx_loop_timeout,
            total_contracts,
            early_exit_clone_rx,
        )
        .await
    });
    let pyrometer_bin_clone = pyrometer_bin.clone();
    let tx_handle = handle.spawn(async move {
        tx_loop(
            fiesta_metadatas,
            tx,
            stop_tx,
            jobs.into(),
            &pyrometer_bin_clone,
            pyrometer_timeout,
            early_exit_clone_tx,
        )
        .await
    });

    // Wait for both tasks to complete
    let (rx_result, _) = runtime.block_on(async { tokio::join!(rx_handle, tx_handle) });

    // Shut down the runtime
    drop(handle);

    // Potentially minimize the smallest source contracts
    if let Some(minimize_path) = args.minimize_path {
        println!("⏹️ Minimizing contracts...");
        let mut smallest_fiesta_metadatas = vec![];
        if let Ok(ref result_distribution) = rx_result {

            for result_message in result_distribution.2.values() {
                smallest_fiesta_metadatas.push(result_message.metadata.clone());
            }
        }
        let early_exit = Arc::new(AtomicBool::new(false));
        let early_exit_clone_rx = early_exit.clone();
        let early_exit_clone_tx = early_exit.clone();

        let (tx, rx) = mpsc::channel();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();

        let handle = runtime.handle().clone();

        let smallest_contracts_len = smallest_fiesta_metadatas.len();
        let rx_handle = handle.spawn(async move {
            minimize_rx_loop(
                rx,
                stop_rx,
                rx_loop_timeout,
                smallest_contracts_len,
                early_exit_clone_rx,
            )
            .await
        });

        let minimize_path_clone = minimize_path.clone();
        let tx_handle = handle.spawn(async move {
            minimize_tx_loop(
                smallest_fiesta_metadatas,
                tx,
                stop_tx,
                jobs.into(),
                &pyrometer_bin,
                pyrometer_timeout,
                early_exit_clone_tx,
                minimize_path_clone.into(),
            )
            .await
        });

        // Wait for both tasks to complete
        let (_, _) = runtime.block_on(async { tokio::join!(rx_handle, tx_handle) });
        // Shut down the runtime
        drop(handle);
        // We don't care about minimized_rx_result, we only want to replace the smallest_source path with the minimized one (if it was successful)
        if let Ok(result_distribution) = rx_result {
            let smallest_sources: HashMap<ExitType, String> = result_distribution.2.iter()
                .map(|(exit_type, result_message)| {
                    let original_path = get_path_str_for_result_message(result_message);
                    let minimized_path = format!("{}/{}.sol", minimize_path, result_message.metadata.bytecode_hash);
                    let final_path = if std::path::Path::new(&minimized_path).exists() {
                        minimized_path
                    } else {
                        original_path
                    };
                    (exit_type.clone(), final_path)
                })
                .collect();

            display_result_distribution(result_distribution.0, result_distribution.1, smallest_sources);
        }
    } else {
        // If we're not minimizing, we can just display the result distribution as is
        if let Ok(result_distribution) = rx_result {
            let smallest_sources: HashMap<ExitType, String> = result_distribution.2.iter()
                .map(|(exit_type, result_message)| (exit_type.clone(), get_path_str_for_result_message(result_message)))
                .collect();

            display_result_distribution(result_distribution.0, result_distribution.1, smallest_sources);
        }
    }

    runtime.shutdown_timeout(Duration::from_secs(2));

    // Call cleanup function to kill any ongoing pyrometer processes. TODO: live server mode would be killed by this
    cleanup();
}

pub async fn tx_loop(
    fiesta_metadatas: Vec<FiestaMetadata>,
    tx_result: mpsc::Sender<ResultMessage>,
    tx_stop: oneshot::Sender<()>,
    max_concurrent_processes: usize,
    pyrometer_bin: &PathBuf,
    pyrometer_timeout: f64,
    early_exit: Arc<AtomicBool>,
) {
    let semaphore = Arc::new(Semaphore::new(max_concurrent_processes));
    let pyrometer_timeout_duration = Duration::from_secs_f64(pyrometer_timeout);
    let mut join_handles = Vec::new();

    for metadata in fiesta_metadatas {
        if early_exit.load(Ordering::Relaxed) {
            break;
        }

        let tx = tx_result.clone();
        let semaphore = semaphore.clone();
        let permit = semaphore.acquire_owned().await;
        let pyrometer_bin_clone = pyrometer_bin.clone();
        let join_handle = tokio::spawn(async move {
            let (mut child, size) = analyze_with_pyrometer(&metadata, &pyrometer_bin_clone);

            let start_time = std::time::Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(_status)) => {
                        let result_message = ResultMessage {
                            metadata: metadata.clone(),
                            child: Some(child),
                            time: start_time.elapsed().as_secs_f64(),
                            size,
                        };
                        let _ = tx.send(result_message);
                        break;
                    }
                    Ok(None) => {
                        if start_time.elapsed() > pyrometer_timeout_duration {
                            let _ = child.kill();
                            let result_message = ResultMessage {
                                metadata: metadata.clone(),
                                child: None,
                                time: pyrometer_timeout,
                                size,
                            };
                            let _ = tx.send(result_message);
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                    Err(e) => {
                        println!("Error while polling child process: {:?}", e);
                        break;
                    }
                }
            }

            drop(permit);
        });

        join_handles.push(join_handle);
    }

    for handle in join_handles {
        let _ = handle.await;
    }

    let _ = tx_stop.send(());
}

pub async fn rx_loop(
    rx_result: mpsc::Receiver<ResultMessage>,
    mut rx_stop: oneshot::Receiver<()>,
    output_path: PathBuf,
    rx_loop_timeout: f64,
    total_contracts: usize,
    early_exit: Arc<AtomicBool>,
) -> (HashMap<ExitType, usize>, usize, HashMap<ExitType, ResultMessage>) {
    let results_writer = ResultsWriter { output_path };
    results_writer.initiate_headers_for_results_csv();

    let rx_loop_timeout = Duration::from_secs_f64(rx_loop_timeout);
    let mut parse_count = 0;
    let mut total_parsable = 0;
    let mut result_distribution = HashMap::new();
    let mut smallest_results = HashMap::new();

    let pb = ProgressBar::new(total_contracts as u64);
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise:.minutes()}] (Time left: {eta}) [{bar:.cyan/blue}] {pos}/{len} {msg}")
        .unwrap()
        .progress_chars("#>-"));

    loop {
        if early_exit.load(Ordering::Relaxed) {
            break;
        }

        match rx_stop.try_recv() {
            Ok(_) => {
                break;
            }
            Err(_) => match rx_result.recv_timeout(rx_loop_timeout) {
                Ok(mut result_message) => {
                    let exit_type = if let Some(child) = result_message.child {
                        check_child_exit(child)
                    } else {
                        ExitType::PerformanceTimeout
                    };
                    result_message.child = None; // We're done with .child, just set it to None so we can clone and stuff without partial borrows from above

                    results_writer.append_to_results_file(
                        &result_message.metadata,
                        &exit_type,
                        result_message.time,
                        result_message.size,
                    );
                    if let ExitType::Success = &exit_type {
                        parse_count += 1;
                    }

                    total_parsable += 1;
                    *result_distribution
                        .entry(exit_type.clone())
                        .or_insert(0) += 1;

                    // Update smallest result for each ExitType, prioritizing SingleMain > Multiple > EtherscanMetadata source types.
                    smallest_results
                        .entry(exit_type.clone())
                        .and_modify(|e: &mut ResultMessage| {
                            let current_priority = match e.metadata.source_type {
                                Some(SourceType::SingleMain(_)) => 0,
                                Some(SourceType::Multiple(_)) => 1,
                                Some(SourceType::EtherscanMetadata(_)) => 2,
                                None => 3,
                            };

                            let new_priority = match result_message.metadata.source_type {
                                Some(SourceType::SingleMain(_)) => 0,
                                Some(SourceType::Multiple(_)) => 1,
                                Some(SourceType::EtherscanMetadata(_)) => 2,
                                None => 3,
                            };

                            if new_priority < current_priority || 
                               (new_priority == current_priority && result_message.size < e.size) {
                                *e = result_message.clone();
                            }
                        })
                        .or_insert_with(|| result_message.clone());

                    pb.inc(1);

                    let success_percent = parse_count as f64 / total_parsable as f64 * 100.0;
                    pb.set_message(format!("(Success: {:.2}%)", success_percent));
                }
                Err(e) => match e {
                    mpsc::RecvTimeoutError::Timeout => {
                        println!("Timeout hit, quitting rx_loop");
                        break;
                    }
                    _ => {
                        println!("Error receiving from rx_result: {:?}", e);
                    }
                },
            },
        }
    }
    (result_distribution, total_parsable, smallest_results)
}

/// A trimmed down version of tx_loop that is only used to run pyrometer to minimize a few contracts
pub async fn minimize_tx_loop(
    fiesta_metadatas: Vec<FiestaMetadata>,
    tx_result: mpsc::Sender<ResultMessage>,
    tx_stop: oneshot::Sender<()>,
    max_concurrent_processes: usize,
    pyrometer_bin: &PathBuf,
    pyrometer_timeout: f64,
    early_exit: Arc<AtomicBool>,
    contract_minimize_output_path: PathBuf,
) {
    let semaphore = Arc::new(Semaphore::new(max_concurrent_processes));
    let pyrometer_timeout_duration = Duration::from_secs_f64(pyrometer_timeout);
    let mut join_handles = Vec::new();

    for metadata in fiesta_metadatas {
        if early_exit.load(Ordering::Relaxed) {
            break;
        }

        let tx = tx_result.clone();
        let semaphore = semaphore.clone();
        let permit = semaphore.acquire_owned().await;
        let pyrometer_bin_clone = pyrometer_bin.clone();
        let contract_minimize_output_path_clone = contract_minimize_output_path.clone();
        let join_handle = tokio::spawn(async move {
            let (mut child, size) = minimize_with_pyrometer(&metadata, &pyrometer_bin_clone, &contract_minimize_output_path_clone);

            let start_time = std::time::Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(_status)) => {
                        let result_message = ResultMessage {
                            metadata: metadata.clone(),
                            child: Some(child),
                            time: start_time.elapsed().as_secs_f64(),
                            size,
                        };
                        let _ = tx.send(result_message);
                        break;
                    }
                    Ok(None) => {
                        if start_time.elapsed() > pyrometer_timeout_duration {
                            let _ = child.kill();
                            let result_message = ResultMessage {
                                metadata: metadata.clone(),
                                child: None,
                                time: pyrometer_timeout,
                                size,
                            };
                            let _ = tx.send(result_message);
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                    Err(e) => {
                        println!("Error while polling child process: {:?}", e);
                        break;
                    }
                }
            }

            drop(permit);
        });

        join_handles.push(join_handle);
    }

    for handle in join_handles {
        let _ = handle.await;
    }

    let _ = tx_stop.send(());
}

/// A trimmed down version of rx_loop that is only used to run pyrometer to minimize a few contracts
pub async fn minimize_rx_loop(
    rx_result: mpsc::Receiver<ResultMessage>,
    mut rx_stop: oneshot::Receiver<()>,
    rx_loop_timeout: f64,
    total_contracts: usize,
    early_exit: Arc<AtomicBool>,
) {
    let rx_loop_timeout = Duration::from_secs_f64(rx_loop_timeout);

    let pb = ProgressBar::new(total_contracts as u64);
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise:.minutes()}] (Time left: {eta}) [{bar:.cyan/blue}] {pos}/{len}")
        .unwrap()
        .progress_chars("#>-"));

    loop {
        if early_exit.load(Ordering::Relaxed) {
            break;
        }

        match rx_stop.try_recv() {
            Ok(_) => {
                break;
            }
            Err(_) => match rx_result.recv_timeout(rx_loop_timeout) {
                Ok(_result_message) => {
                    pb.inc(1);
                }
                Err(e) => match e {
                    mpsc::RecvTimeoutError::Timeout => {
                        println!("Timeout hit, quitting rx_loop");
                        break;
                    }
                    _ => {
                        println!("Error receiving from rx_result: {:?}", e);
                    }
                },
            },
        }
    }
}

fn cleanup() {
    let mut system = System::new_all();
    system.refresh_all();

    for (pid, process) in system.processes() {
        if process.name() == "pyrometer" {
            println!("🔪 Killing hanging pyrometer process: {:?}", process);
            if !process.kill() {
                eprintln!("Failed to kill process {}", pid);
            }
        }
    }
}
