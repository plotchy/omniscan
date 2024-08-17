use crate::{
    data_structures::{ExitType, SourceType},
    FiestaMetadata,
};
use ethers::etherscan::contract::SourceCodeMetadata;
use indicatif::{ProgressBar, ProgressStyle};
use lazy_static::lazy_static;
use prettytable::{Cell, Row, Table};
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{
    fs,
    process::{Command, Stdio},
};
use std::{panic, process::Child};
use walkdir::WalkDir;

const FIESTA_TOTAL_CONTRACTS: usize = 150_000;
const DEFAULT_NUM_CONTRACTS: usize = 1000;
const DEFAULT_TIMEOUT_SECS: f64 = 1.0;


lazy_static! {
    /// Thread panic, but is Todo based
    static ref TODO_ERROR_REGEX: Regex =
        Regex::new(r#"thread '.*?' panicked at .+?\nEncountered an error: Todo\(File\(\d+, \d+, \d+\), \"([\w\s]*)"#).unwrap();

    /// Thread panic, but is Parse based
    static ref PARSE_ERROR_REGEX: Regex =
        Regex::new(r#"thread '.*?' panicked at .+?\nEncountered an error: ParseError\(File\(\d+, \d+, \d+\), \"([\w\s]*)"#).unwrap();

    /// Thread panic, but is any pyrometer error-catching
    static ref DEBUG_PANIC_REGEX: Regex =
        Regex::new(r#"thread '.*?' panicked at .+?\nEncountered an error: (\w+)"#).unwrap();

    /// Thread panic in complete catch-all form
    static ref PANIC_REGEX: Regex = Regex::new(r"thread '.*?' panicked at (.+?)\n").unwrap();

    /// Thread overflowed its stack (not a panic but is still fatal)
    static ref STACK_OVERFLOW_REGEX: Regex =
        Regex::new(r"thread '.*?' has overflowed its stack\n").unwrap();

    /// Pyrometer error that didn't cause a thread panic
    static ref ERROR_REGEX: Regex = Regex::new(r"(?s)Error:.*?31m([a-zA-Z0-9` .]{5,})").unwrap();

    /// Success!
    static ref SUCCESS_REGEX: Regex =
        Regex::new(r"DONE ANALYZING IN: \d+ms\. Writing to cli\.\.\.\n$").unwrap();
}


pub fn collect_fiesta_metadatas(
    abs_fiesta_path: &Path,
    num_contracts: usize,
    skip_contracts: usize,
) -> Vec<FiestaMetadata> {
    let mut fiesta_metadatas = Vec::with_capacity(FIESTA_TOTAL_CONTRACTS);
    let mut contract_count = 0;
    let mut skipped_count = 0;
    println!("🚚 Gathering {} contracts", num_contracts);
    let pb = ProgressBar::new(num_contracts as u64);
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise:.minutes()}] (Time left: {eta}) [{bar:.cyan/blue}] {pos}/{len} {msg}")
        .unwrap()
        .progress_chars("#>-"));

    for entry in WalkDir::new(abs_fiesta_path.join("organized_contracts")) {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_file() && path.file_name().unwrap() == "metadata.json" {
            let file = fs::File::open(path).unwrap();
            let mut metadata: FiestaMetadata = serde_json::from_reader(file).unwrap();
            if !metadata.compiler_is_supported() {
                continue;
            }

            if skipped_count < skip_contracts {
                skipped_count += 1;
                continue;
            }

            let mut path_to_dir = path.to_path_buf();
            path_to_dir.pop();
            metadata.update_path_to_dir(&path_to_dir);
            collect_contract_sources(&mut metadata);

            if metadata.source_type.is_some() {
                fiesta_metadatas.push(metadata);
                contract_count += 1;
                pb.inc(1);

                if contract_count == num_contracts {
                    break;
                }
            }
        }
    }

    pb.finish();
    fiesta_metadatas
}

fn collect_contract_sources(metadata: &mut FiestaMetadata) {
    /*
    There will either be a main.sol file, several .sol files of different names, or a contracts.json file
    - first look for contracts.json
    - then look for one .sol file named main.sol
    - then look for multiple .sol files
    - edgecase is a single main.vy file that has misconfigured metadata.json... there's about 10 of these, we can skip.
    */
    let path_to_dir = std::path::PathBuf::from(&metadata.abs_path_to_dir);
    let mut path_to_contract = std::path::PathBuf::new();
    for entry in WalkDir::new(&path_to_dir) {
        let entry = entry.unwrap();
        let path = entry.path();
        // println!("Looking for contracts.json: {}", &path.display());
        if path.is_file() && path.file_name().unwrap() == "contract.json" {
            path_to_contract = path.to_path_buf();
            let json_string = std::fs::read_to_string(path_to_contract.clone()).unwrap();
            // println!("{:#?}", &json_string);
            let contract_metadata: SourceCodeMetadata = serde_json::from_str(&json_string).unwrap();
            metadata.update_source_type(SourceType::EtherscanMetadata(contract_metadata));
            break;
        }
    }
    // if contracts.json wasnt found, look for multiple .sol files
    if path_to_contract == std::path::PathBuf::new() {
        let mut sol_files = Vec::new();
        for entry in WalkDir::new(&path_to_dir) {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_file() && path.extension().unwrap() == "sol" {
                sol_files.push(path.to_path_buf());
            }
        }
        // if there is only one .sol file, use that

        if sol_files.len() == 1 {
            path_to_contract = sol_files[0].to_path_buf();
            metadata.update_source_type(SourceType::SingleMain(
                std::fs::read_to_string(path_to_contract.clone()).unwrap(),
            ));
        } else if sol_files.is_empty() {
            // println!("Found no .sol files: {}. this is likely a main.vy that should be a main.sol. needs changed", &path_to_dir.display())
            // could go to path_to_contract and rename main.vy to main.sol
        } else {
            // if there are multiple .sol files, look for main.sol
            let mut multiple_files = sol_files
                .into_iter()
                .map(|path| {
                    let name = path.file_name().unwrap().to_str().unwrap().to_string();
                    let string = std::fs::read_to_string(path).unwrap();
                    (name, string)
                })
                .collect::<Vec<(String, String)>>();
            multiple_files.sort_by(|a, b| a.0.cmp(&b.0));
            metadata.update_source_type(SourceType::Multiple(multiple_files));
        }
    }
}

pub fn analyze_with_pyrometer(metadata: &FiestaMetadata, pyrometer_bin: &PathBuf) -> (Child, u64) {
    match metadata.clone().source_type.unwrap() {
        SourceType::SingleMain(_sol) => {
            let path_to_file = PathBuf::from(metadata.abs_path_to_dir.clone()).join("main.sol");
            let path_to_file = path_to_file.to_str().unwrap();
            let size = fs::metadata(path_to_file).unwrap().len();

            let child = Command::new(&pyrometer_bin)
                .args([path_to_file, "--debug", "--debug-panic"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("Failed to spawn process");

            (child, size)
        }
        SourceType::Multiple(multiple_files) => {
            let substr_to_find = format!("contract {} ", metadata.contract_name);
            for (name, sol_string) in multiple_files {
                if sol_string.contains(&substr_to_find) {
                    let path_to_file = PathBuf::from(metadata.abs_path_to_dir.clone()).join(name);
                    let path_to_file = path_to_file.to_str().unwrap();
                    let size = fs::metadata(path_to_file).unwrap().len();

                    let child = Command::new(&pyrometer_bin)
                        .args([path_to_file, "--debug"])
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .expect("Failed to spawn process");

                    return (child, size);
                }
            }
            panic!(
                "Could not find contract name {} in multiple_files",
                metadata.contract_name
            );
        }
        SourceType::EtherscanMetadata(_source_metadata) => {
            let path_to_file =
                PathBuf::from(metadata.abs_path_to_dir.clone()).join("contract.json");
            let path_to_file = path_to_file.to_str().unwrap();
            let size = fs::metadata(path_to_file).unwrap().len();
            let child = Command::new(&pyrometer_bin)
                .args([path_to_file, "--debug"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("Failed to spawn process");

            (child, size)
        }
    }
}

pub fn check_child_exit(child: Child) -> ExitType {
    // determine if the exit status has panics, errors, etc.
    if child.stdout.is_some() & child.stderr.is_some() {
        let stdout = child.stdout.unwrap();
        let mut stdout_reader = std::io::BufReader::new(stdout);
        let mut stdout_string = String::new();
        std::io::Read::read_to_string(&mut stdout_reader, &mut stdout_string).unwrap();
        let mut stderr = child.stderr.unwrap();
        let mut stderr_string = String::new();
        std::io::Read::read_to_string(&mut stderr, &mut stderr_string).unwrap();

        // convert stdout into one of the ExitType variants
        convert_pyrometer_output_to_exit_type(stdout_string, stderr_string)
    } else {
        dbg!(&child);
        panic!("Child stdout is None")
    }
}

fn convert_pyrometer_output_to_exit_type(stdout_string: String, stderr_string: String) -> ExitType {
    // Check if the output is from stderr and contains the phrase "thread 'main' panicked at"
    if let Some(todo_err) = TODO_ERROR_REGEX.captures(&stderr_string) {
        return ExitType::Error(format!("Todo: {}", &todo_err[1]));
    }
    if let Some(parse_err) = PARSE_ERROR_REGEX.captures(&stderr_string) {
        return ExitType::Error(parse_err[1].to_string());
    }
    if let Some(debug_panic_err) = DEBUG_PANIC_REGEX.captures(&stderr_string) {
        return ExitType::ThreadPanic(debug_panic_err[1].to_string());
    }

    if let Some(captures) = PANIC_REGEX.captures(&stderr_string) {
        return ExitType::ThreadPanic(captures[1].to_string());
    }

    if let Some(_captures) = STACK_OVERFLOW_REGEX.captures(&stderr_string) {
        return ExitType::ThreadPanic("Stack overflow".to_string());
    }

    // Check if the output is from stdout and contains an error message
    if let Some(captures) = ERROR_REGEX.captures(&stdout_string) {
        let error_message = captures[1].trim().to_string();
        return ExitType::Error(error_message);
    }

    // Check if the output is from stdout and contains a success message
    if SUCCESS_REGEX.is_match(&stdout_string) {
        return ExitType::Success;
    }

    // If none of the above patterns are matched, return a NonInterpreted variant.
    ExitType::NonInterpreted(stdout_string, stderr_string)
}

pub fn display_result_distribution(distribution: HashMap<String, usize>, total: usize) {
    let mut table = Table::new();
    table.add_row(Row::new(vec![
        Cell::new("Result Type"),
        Cell::new("Count"),
        Cell::new("Percentage"),
    ]));

    let mut sorted_distribution: Vec<_> = distribution.into_iter().collect();
    sorted_distribution.sort_by(|a, b| b.1.cmp(&a.1));

    for (result_type, count) in sorted_distribution {
        let percentage = (count as f64 / total as f64) * 100.0;
        table.add_row(Row::new(vec![
            Cell::new(&result_type),
            Cell::new(&count.to_string()),
            Cell::new(&format!("{:.2}%", percentage)),
        ]));
    }

    table.printstd();
}

pub fn get_output_path(output: Option<String>) -> PathBuf {
    match output {
        Some(path) => {
            let path = PathBuf::from(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            path
        }
        None => {
            let mut path = PathBuf::from("./data");
            path.push(format!(
                "results_{}.csv",
                chrono::Local::now().format("%m-%d_%H-%M")
            ));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            path
        }
    }
}

/// First return value is pyrometer per-run timeout, second return value is rx_loop waiting period before early-exiting
pub fn get_timeouts(timeout_secs: Option<f64>) -> (f64, f64) {
    match timeout_secs {
        Some(timeout) => {
            if timeout == 0.0 {
                (1_000_000.0, 1_000_000.0)
            } else {
                (timeout, timeout + 1.0)
            }
        }
        None => (DEFAULT_TIMEOUT_SECS, DEFAULT_TIMEOUT_SECS + 1.0),
    }
}

pub fn get_num_contracts(num_contracts: Option<usize>) -> usize {
    match num_contracts {
        Some(num_contracts) => {
            if num_contracts == 0 {
                usize::MAX
            } else {
                num_contracts
            }
        }
        None => DEFAULT_NUM_CONTRACTS,
    }
}

pub fn build_pyrometer(pyrometer_manifest: &PathBuf) -> PathBuf {
    let manifest_dir = pyrometer_manifest.parent().expect("Invalid manifest path");
    
    let status = Command::new("cargo")
        .current_dir(manifest_dir)
        .args(["build", "--release"])
        .status()
        .expect("Failed to build Pyrometer");

    if !status.success() {
        panic!("Failed to build Pyrometer");
    }

    pyrometer_manifest
        .parent()
        .unwrap()
        .join("target")
        .join("release")
        .join("pyrometer")
}
