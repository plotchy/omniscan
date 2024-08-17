use ethers::etherscan::contract::SourceCodeMetadata;
use serde::{Deserialize, Serialize};
use std::{fmt, fs::OpenOptions, io::Write, path::PathBuf, process::Child};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum SourceType {
    SingleMain(String),
    Multiple(Vec<(String, String)>),
    EtherscanMetadata(SourceCodeMetadata),
}

impl fmt::Display for SourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceType::SingleMain(_) => write!(f, "SingleFile"),
            SourceType::Multiple(_) => write!(f, "MultipleFiles"),
            SourceType::EtherscanMetadata(_) => write!(f, "JSON"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FiestaMetadata {
    #[serde(rename = "ContractName")]
    pub contract_name: String,
    #[serde(rename = "CompilerVersion")]
    pub compiler_version: String,
    #[serde(rename = "Runs")]
    pub runs: i64,
    #[serde(rename = "OptimizationUsed")]
    pub optimization_used: bool,
    #[serde(rename = "BytecodeHash")]
    pub bytecode_hash: String,
    #[serde(skip_serializing, skip_deserializing)]
    pub abs_path_to_dir: String,
    #[serde(skip_serializing, skip_deserializing)]
    pub source_type: Option<SourceType>,
}

impl FiestaMetadata {
    pub fn compiler_is_supported(&self) -> bool {
        self.compiler_version.starts_with("v0.8.") && !self.compiler_version.contains("vyper")
    }

    pub fn update_path_to_dir(&mut self, path_to_dir: &std::path::Path) {
        self.abs_path_to_dir = path_to_dir.to_str().unwrap().to_string();
    }

    pub fn update_source_type(&mut self, source_type: SourceType) {
        self.source_type = Some(source_type);
    }
}

pub struct ResultMessage {
    pub metadata: FiestaMetadata,
    pub child: Option<Child>,
    pub time: f64,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub enum ExitType {
    Success,
    PerformanceTimeout,
    Error(String),
    ThreadPanic(String),
    NonInterpreted(String, String),
}

impl fmt::Display for ExitType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitType::Success => write!(f, "Success"),
            ExitType::PerformanceTimeout => write!(f, "PerformanceTimeout"),
            ExitType::Error(s) => write!(f, "Error: {}", s.replace(',', ":")),
            ExitType::ThreadPanic(s) => write!(f, "ThreadPanic: {}", s.replace(',', ":")),
            ExitType::NonInterpreted(_stdout, _stderr) => write!(f, "NonInterpreted Error"),
        }
    }
}

pub struct ResultsWriter {
    pub output_path: PathBuf,
}
impl ResultsWriter {
    pub fn convert_fields_to_header() -> String {
        "bytecode_hash,result,time (sec),source_type,source_size\n".to_string()
    }

    pub fn initiate_headers_for_results_csv(&self) {
        println!(
            "📝 Ongoing results will be written to: {:?}",
            &self.output_path
        );
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.output_path)
            .unwrap();

        let header_string = Self::convert_fields_to_header();
        file.write_all(header_string.as_bytes()).unwrap();
    }

    pub fn append_to_results_file(
        &self,
        metadata: &FiestaMetadata,
        exit_type: &ExitType,
        time: f64,
        size: u64,
    ) {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.output_path)
            .unwrap();

        let bytecode_hash = metadata.bytecode_hash.clone();
        let source_type = metadata.source_type.clone().unwrap();

        let result_row =
            ResultsRow::from(exit_type.clone(), bytecode_hash, source_type, time, size);

        let row_string = result_row.convert_to_csv_string();

        file.write_all(row_string.as_bytes()).unwrap();
    }
}

pub struct ResultsRow {
    pub bytecode_hash: String,
    pub result: ExitType,
    pub time: f64,
    pub source_type: SourceType,
    pub size: u64,
}

impl ResultsRow {
    pub fn from(
        result: ExitType,
        bytecode_hash: String,
        source_type: SourceType,
        time: f64,
        size: u64,
    ) -> Self {
        Self {
            bytecode_hash,
            result: result.clone(),
            time,
            source_type,
            size,
        }
    }

    pub fn convert_to_csv_string(&self) -> String {
        format!(
            "{},{},{:.3},{},{}\n",
            self.bytecode_hash, self.result, self.time, self.source_type, self.size
        )
    }
}
