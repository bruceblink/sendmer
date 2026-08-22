use sendmer::{RelayModeOption, SendOptions, send};
use std::{
    env,
    fs::File,
    io::{self, Write},
    num::NonZeroU64,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tempfile::{TempDir, tempdir};

const DEFAULT_ITERATIONS: usize = 3;
const DEFAULT_FILE_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_DIRECTORY_FILES: usize = 256;
const DEFAULT_DIRECTORY_FILE_BYTES: u64 = 64 * 1024;
const WRITE_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
enum BenchmarkCase {
    LargeFile,
    LargeDirectory,
}

impl BenchmarkCase {
    const fn name(self) -> &'static str {
        match self {
            Self::LargeFile => "large-file",
            Self::LargeDirectory => "large-directory",
        }
    }
}

#[derive(Clone, Copy)]
struct BenchmarkConfig {
    iterations: usize,
    file_bytes: u64,
    directory_files: usize,
    directory_file_bytes: u64,
    import_memory_bytes: Option<NonZeroU64>,
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .unwrap_or_else(|error| panic!("{name} must be an integer: {error}"))
        })
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<u64>()
                .unwrap_or_else(|error| panic!("{name} must be an integer: {error}"))
        })
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn benchmark_config() -> BenchmarkConfig {
    let import_memory_bytes = env::var("SENDMER_BENCH_IMPORT_MEMORY_BYTES")
        .ok()
        .map(|value| {
            let bytes = value.parse::<u64>().unwrap_or_else(|error| {
                panic!("SENDMER_BENCH_IMPORT_MEMORY_BYTES must be an integer: {error}")
            });
            NonZeroU64::new(bytes).unwrap_or_else(|| {
                panic!("SENDMER_BENCH_IMPORT_MEMORY_BYTES must be greater than zero")
            })
        });

    BenchmarkConfig {
        iterations: env_usize("SENDMER_BENCH_ITERATIONS", DEFAULT_ITERATIONS),
        file_bytes: env_u64("SENDMER_BENCH_FILE_BYTES", DEFAULT_FILE_BYTES),
        directory_files: env_usize("SENDMER_BENCH_DIRECTORY_FILES", DEFAULT_DIRECTORY_FILES),
        directory_file_bytes: env_u64(
            "SENDMER_BENCH_DIRECTORY_FILE_BYTES",
            DEFAULT_DIRECTORY_FILE_BYTES,
        ),
        import_memory_bytes,
    }
}

/// Write deterministic bytes so each benchmark measures real file import work rather than setup.
fn write_pattern(path: &Path, bytes: u64) -> io::Result<()> {
    let mut file = File::create(path)?;
    let chunk = [0x5a_u8; WRITE_CHUNK_BYTES];
    let mut remaining = bytes;
    while remaining > 0 {
        let length = remaining.min(chunk.len() as u64) as usize;
        file.write_all(&chunk[..length])?;
        remaining -= length as u64;
    }
    file.sync_all()
}

fn create_source(
    case: BenchmarkCase,
    root: &TempDir,
    config: BenchmarkConfig,
) -> io::Result<(PathBuf, usize, u64)> {
    match case {
        BenchmarkCase::LargeFile => {
            let path = root.path().join("large-file.bin");
            write_pattern(&path, config.file_bytes)?;
            Ok((path, 1, config.file_bytes))
        }
        BenchmarkCase::LargeDirectory => {
            let path = root.path().join("large-directory");
            std::fs::create_dir(&path)?;
            for index in 0..config.directory_files {
                write_pattern(
                    &path.join(format!("file-{index:04}.bin")),
                    config.directory_file_bytes,
                )?;
            }
            let total = config
                .directory_file_bytes
                .saturating_mul(config.directory_files as u64);
            Ok((path, config.directory_files, total))
        }
    }
}

fn average(durations: &[Duration]) -> Duration {
    let total_nanos: u128 = durations.iter().map(Duration::as_nanos).sum();
    Duration::from_nanos((total_nanos / durations.len() as u128) as u64)
}

/// Run sender setup for each scale case and print raw timings for later comparison.
async fn run_case(case: BenchmarkCase, config: BenchmarkConfig) -> anyhow::Result<()> {
    let mut timings = Vec::with_capacity(config.iterations);
    for iteration in 1..=config.iterations {
        let source_root = tempdir()?;
        let (source, files, bytes) = create_source(case, &source_root, config)?;
        let options = SendOptions {
            relay_mode: RelayModeOption::Disabled,
            max_import_memory_bytes: config.import_memory_bytes,
            ..SendOptions::default()
        };

        let started = Instant::now();
        let sender = send(source, options, None).await?;
        let elapsed = started.elapsed();
        sender.shutdown().await?;
        timings.push(elapsed);
        println!(
            "case={} iteration={} files={} bytes={} setup_ms={:.3}",
            case.name(),
            iteration,
            files,
            bytes,
            elapsed.as_secs_f64() * 1_000.0
        );
    }

    println!(
        "case={} summary=min_ms:{:.3},avg_ms:{:.3},max_ms:{:.3}",
        case.name(),
        timings
            .iter()
            .min()
            .expect("benchmark timings")
            .as_secs_f64()
            * 1_000.0,
        average(&timings).as_secs_f64() * 1_000.0,
        timings
            .iter()
            .max()
            .expect("benchmark timings")
            .as_secs_f64()
            * 1_000.0,
    );
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = benchmark_config();
    println!(
        "sendmer scale benchmark os={} arch={} iterations={} file_bytes={} directory_files={} directory_file_bytes={} import_memory_bytes={:?}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        config.iterations,
        config.file_bytes,
        config.directory_files,
        config.directory_file_bytes,
        config.import_memory_bytes.map(NonZeroU64::get),
    );
    run_case(BenchmarkCase::LargeFile, config).await?;
    run_case(BenchmarkCase::LargeDirectory, config).await
}
