//! Three-mode, fresh-process trials. See docs/benchmarks.md for interpretation.
mod support;

use epochsnap::{Arena, CaptureMode, CheckpointStatus};
use std::{
    collections::HashMap,
    fs::File,
    hint::black_box,
    io::{BufWriter, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use support::{Result, arena_words, hash, mix, page_size};

const POLL_BATCH: usize = 64;
const HEADER: &str = "mode,workload,seed,shuffle_seed,trial,run_index,arena_bytes,page_bytes,operations,checkpoint_at,poll_batch_ops,first_write_pending_pages,arena_init_ns,workload_init_ns,observer_setup_ns,checkpoint_call_ns,boundary_offset_ns,capture_complete_ns,t_to_ready_ns,worker_elapsed_ns,worker_cpu_ns,wait_ns,application_ns,operations_per_second,op_p50_ns,op_p95_ns,op_p99_ns,op_max_ns,first_write_p50_ns,first_write_p95_ns,first_write_p99_ns,first_write_max_ns,fault_pages,scan_pages,copied_bytes,restore_ns,restore_validation_ns,validation_ns,discard_ns,arena_drop_ns,trial_total_ns,process_cpu_ns,voluntary_switches,involuntary_switches,application_process_cpu_ns,application_voluntary_switches,application_involuntary_switches,peak_rss_kib,trial_peak_rss_kib,state_hash,image_hash,clock_pair_p50_ns,child_wall_ns";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    None,
    Stopped,
    Wp,
}
impl Mode {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "stopped" => Ok(Self::Stopped),
            "wp" => Ok(Self::Wp),
            _ => Err(format!("unknown mode {value}").into()),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Stopped => "stopped",
            Self::Wp => "wp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    ReadHeavy,
    Sparse,
    Clustered,
    Sequential,
}
impl Workload {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "read-heavy" => Ok(Self::ReadHeavy),
            "sparse" => Ok(Self::Sparse),
            "clustered" => Ok(Self::Clustered),
            "sequential" => Ok(Self::Sequential),
            _ => Err(format!("unknown workload {value}").into()),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::ReadHeavy => "read-heavy",
            Self::Sparse => "sparse",
            Self::Clustered => "clustered",
            Self::Sequential => "sequential",
        }
    }
    fn writes(self) -> bool {
        self != Self::ReadHeavy
    }
    fn page(self, seed: u64, op: usize, pages: usize) -> usize {
        match self {
            Self::Sequential => op % pages,
            Self::Clustered => mix(seed ^ op as u64) as usize % pages.div_ceil(100),
            _ => mix(seed ^ op as u64) as usize % pages,
        }
    }
    fn slots(self, seed: u64, op: usize, page_words: usize) -> std::ops::Range<usize> {
        match self {
            Self::Sequential => 0..page_words,
            Self::ReadHeavy => 0..8,
            _ => {
                let slot = mix(seed.wrapping_add(op as u64)) as usize % page_words;
                slot..slot + 1
            }
        }
    }
}

struct Config {
    mib: usize,
    workload: Workload,
    seed: u64,
    shuffle_seed: u64,
    trials: usize,
    operations: usize,
    checkpoint_at: usize,
    output: Option<String>,
    modes: Vec<Mode>,
    child: Option<Mode>,
    trial: usize,
    run_index: usize,
}
impl Config {
    fn parse() -> Result<Self> {
        let mut config = Self {
            mib: 64,
            workload: Workload::Sparse,
            seed: 1,
            shuffle_seed: 1,
            trials: 10,
            operations: 0,
            checkpoint_at: 0,
            output: None,
            modes: vec![Mode::None, Mode::Stopped, Mode::Wp],
            child: None,
            trial: 0,
            run_index: 0,
        };
        let mut args = std::env::args().skip(1);
        while let Some(key) = args.next() {
            if key == "--help" {
                println!(
                    "bench --arena-mib N --workload read-heavy|sparse|clustered|sequential --seed N --trials N --output FILE [--shuffle-seed N] [--operations N --checkpoint-at N] [--modes none,stopped,wp]\nDefaults: 64 MiB, sparse, seeds 1, 10 trials; 3 operations/page, checkpoint after 1 operation/page. Explicit modes select coverage; WP never falls back."
                );
                std::process::exit(0);
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {key}"))?;
            match key.as_str() {
                "--arena-mib" => config.mib = value.parse()?,
                "--workload" => config.workload = Workload::parse(&value)?,
                "--seed" => config.seed = value.parse()?,
                "--shuffle-seed" => config.shuffle_seed = value.parse()?,
                "--trials" => config.trials = value.parse()?,
                "--operations" => {
                    config.operations = value.parse()?;
                    if config.operations == 0 {
                        return Err("operations must be positive".into());
                    }
                }
                "--checkpoint-at" => {
                    config.checkpoint_at = value.parse()?;
                    if config.checkpoint_at == 0 {
                        return Err("checkpoint boundary must be positive".into());
                    }
                }
                "--output" => config.output = Some(value),
                "--modes" => {
                    config.modes = value.split(',').map(Mode::parse).collect::<Result<_>>()?
                }
                "--internal-trial" => config.child = Some(Mode::parse(&value)?),
                "--trial" => config.trial = value.parse()?,
                "--run-index" => config.run_index = value.parse()?,
                _ => return Err(format!("unknown option {key}").into()),
            }
        }
        let words = arena_words(config.mib)?;
        let page = page_size()?;
        let pages = (words * 8).div_ceil(page);
        if config.operations == 0 {
            config.operations = pages.checked_mul(3).ok_or("operation overflow")?;
        }
        if config.checkpoint_at == 0 {
            config.checkpoint_at = pages;
        }
        if config.trials == 0 || config.checkpoint_at >= config.operations {
            return Err("trials must be positive; checkpoint must precede final operation".into());
        }
        config
            .trials
            .checked_mul(config.modes.len())
            .ok_or("trial count overflow")?;
        for (i, mode) in config.modes.iter().enumerate() {
            if config.modes[..i].contains(mode) {
                return Err("duplicate modes".into());
            }
        }
        Ok(config)
    }
}

fn value(seed: u64, op: usize, slot: usize) -> u64 {
    mix(seed ^ (op as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (slot as u64).rotate_left(19))
}

#[derive(Clone, Copy)]
struct Usage {
    cpu_ns: u128,
    voluntary: i64,
    involuntary: i64,
    peak_kib: i64,
}
fn usage() -> Result<Usage> {
    let mut raw = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage writes the entire correctly aligned rusage on success;
    // RUSAGE_SELF includes this process's owner and worker, no arena pointer.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, raw.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: the successful call initialized all fields of raw.
    let raw = unsafe { raw.assume_init() };
    let ns =
        |time: libc::timeval| time.tv_sec as u128 * 1_000_000_000 + time.tv_usec as u128 * 1000;
    Ok(Usage {
        cpu_ns: ns(raw.ru_utime) + ns(raw.ru_stime),
        voluntary: raw.ru_nvcsw,
        involuntary: raw.ru_nivcsw,
        peak_kib: raw.ru_maxrss,
    })
}

fn percentile(sorted: &[u128], percent: usize) -> Option<u128> {
    if sorted.is_empty() {
        None
    } else {
        Some(sorted[(sorted.len() * percent).div_ceil(100).saturating_sub(1)])
    }
}
fn number(value: impl ToString) -> String {
    value.to_string()
}
fn optional(value: Option<u128>) -> String {
    value.map(number).unwrap_or_else(|| "NA".into())
}
fn duration(value: Option<Duration>) -> String {
    optional(value.map(|d| d.as_nanos()))
}

fn trial(config: &Config, mode: Mode) -> Result<String> {
    let started = Instant::now();
    let process_before = usage()?;
    let page = page_size()?;
    let init = Instant::now();
    let mut arena = Arena::new(arena_words(config.mib)?)?;
    let arena_init = init.elapsed();
    let words = arena.len_words();
    let page_words = page / 8;
    let pages = words / page_words;
    let init = Instant::now();
    for slot in 0..words {
        arena.store_word(slot, mix(config.seed ^ slot as u64))?;
    }
    let workload_init = init.elapsed();
    let setup = Instant::now();
    let mut latencies = Vec::with_capacity(config.operations);
    let mut first_latencies = Vec::with_capacity(pages.min(config.operations));
    let mut written = vec![false; pages];
    let mut clocks: Vec<_> = (0..10_000)
        .map(|_| Instant::now().elapsed().as_nanos())
        .collect();
    clocks.sort_unstable();
    let clock_pair = percentile(&clocks, 50).unwrap();
    let observer_setup = setup.elapsed();
    let mut epoch = None;
    let mut pending = false;
    let mut metrics = None;
    let mut call_time = None;
    let mut checksum = 0u64;
    let app_before = usage()?;
    let app = Instant::now();
    for op in 0..config.operations {
        if op == config.checkpoint_at && mode != Mode::None {
            let at = Instant::now();
            epoch = Some(arena.checkpoint(match mode {
                Mode::Stopped => CaptureMode::Stopped,
                _ => CaptureMode::WriteProtected,
            })?);
            call_time = Some(at.elapsed());
            pending = mode == Mode::Wp;
        }
        let target_page = config.workload.page(config.seed, op, pages);
        let first_pending_write = pending && config.workload.writes() && !written[target_page];
        if op >= config.checkpoint_at && config.workload.writes() {
            written[target_page] = true;
        }
        let operation = Instant::now();
        for offset in config.workload.slots(config.seed, op, page_words) {
            let slot = target_page * page_words + offset;
            if config.workload.writes() {
                arena.store_word(slot, value(config.seed, op, slot))?;
            } else {
                checksum ^= black_box(arena.load_word(slot)?);
            }
        }
        let elapsed = operation.elapsed().as_nanos();
        latencies.push(elapsed);
        if first_pending_write {
            first_latencies.push(elapsed);
        }
        if (op + 1) % POLL_BATCH == 0
            && let Some(epoch) = epoch
        {
            match arena.checkpoint_status(epoch)? {
                CheckpointStatus::Pending { .. } => {}
                CheckpointStatus::Ready { metrics: ready } => {
                    metrics = Some(ready);
                    pending = false;
                }
            }
        }
    }
    let application = app.elapsed();
    let app_after = usage()?;
    black_box(checksum);
    let wait = Instant::now();
    if let Some(epoch) = epoch {
        arena.wait_checkpoint(epoch)?;
        let CheckpointStatus::Ready { metrics: ready } = arena.checkpoint_status(epoch)? else {
            return Err("wait did not finalize checkpoint".into());
        };
        metrics = Some(ready);
    }
    let wait = epoch.map(|_| wait.elapsed());
    let capture_usage = usage()?; // Peak before allocating the validation model.
    let validation = Instant::now();
    let mut expected: Vec<_> = (0..words)
        .map(|slot| mix(config.seed ^ slot as u64))
        .collect();
    let mut image_hash = None;
    for op in 0..=config.operations {
        if op == config.checkpoint_at
            && let Some(epoch) = epoch
        {
            let image = arena.checkpoint_words(epoch)?;
            if image != expected {
                return Err("checkpoint full-image mismatch".into());
            }
            image_hash = Some(hash(image.iter().copied()));
        }
        if op == config.operations {
            break;
        }
        if config.workload.writes() {
            let target_page = config.workload.page(config.seed, op, pages);
            for offset in config.workload.slots(config.seed, op, page_words) {
                let slot = target_page * page_words + offset;
                expected[slot] = value(config.seed, op, slot);
            }
        }
    }
    for (slot, &value) in expected.iter().enumerate() {
        if arena.load_word(slot)? != value {
            return Err(format!("live state mismatch at slot {slot}").into());
        }
    }
    let state_hash = hash(expected.iter().copied());
    drop(expected);
    let validation = validation.elapsed();
    let mut restore = None;
    let mut restore_validation = None;
    let mut discard = None;
    if let Some(epoch) = epoch {
        let at = Instant::now();
        arena.restore(epoch)?;
        restore = Some(at.elapsed());
        let at = Instant::now();
        for (slot, &value) in arena.checkpoint_words(epoch)?.iter().enumerate() {
            if arena.load_word(slot)? != value {
                return Err("restored full-state mismatch".into());
            }
        }
        restore_validation = Some(at.elapsed());
        let at = Instant::now();
        arena.discard_checkpoint(epoch)?;
        discard = Some(at.elapsed());
    }
    let at = Instant::now();
    drop(arena);
    let arena_drop = at.elapsed();
    latencies.sort_unstable();
    first_latencies.sort_unstable();
    let process_after = usage()?;
    let trial_total = started.elapsed();
    if let Some(metrics) = metrics
        && (metrics.fault_pages + metrics.scan_pages != pages || metrics.copied_bytes != words * 8)
    {
        return Err("capture accounting mismatch".into());
    }
    let mut fields = vec![
        number(mode.name()),
        number(config.workload.name()),
        number(config.seed),
        number(config.shuffle_seed),
        number(config.trial),
        number(config.run_index),
        number(words * 8),
        number(page),
        number(config.operations),
        number(config.checkpoint_at),
        number(POLL_BATCH),
        if epoch.is_some() {
            number(first_latencies.len())
        } else {
            "NA".into()
        },
        number(arena_init.as_nanos()),
        number(workload_init.as_nanos()),
        number(observer_setup.as_nanos()),
        duration(call_time),
        optional(metrics.map(|m| m.boundary_offset.as_nanos())),
        optional(metrics.map(|m| m.ready_offset.as_nanos())),
        optional(metrics.map(|m| (m.ready_offset - m.boundary_offset).as_nanos())),
        duration(metrics.and_then(|m| m.worker_elapsed)),
        duration(metrics.and_then(|m| m.worker_cpu)),
        duration(wait),
        number(application.as_nanos()),
        number(config.operations as f64 / application.as_secs_f64()),
    ];
    for samples in [&latencies, &first_latencies] {
        for percent in [50, 95, 99, 100] {
            fields.push(optional(percentile(samples, percent)));
        }
    }
    fields.extend([
        metrics
            .map(|m| number(m.fault_pages))
            .unwrap_or("NA".into()),
        metrics.map(|m| number(m.scan_pages)).unwrap_or("NA".into()),
        metrics
            .map(|m| number(m.copied_bytes))
            .unwrap_or("NA".into()),
        duration(restore),
        duration(restore_validation),
        number(validation.as_nanos()),
        duration(discard),
        number(arena_drop.as_nanos()),
        number(trial_total.as_nanos()),
        number(process_after.cpu_ns - process_before.cpu_ns),
        number(process_after.voluntary - process_before.voluntary),
        number(process_after.involuntary - process_before.involuntary),
        number(app_after.cpu_ns - app_before.cpu_ns),
        number(app_after.voluntary - app_before.voluntary),
        number(app_after.involuntary - app_before.involuntary),
        number(capture_usage.peak_kib),
        number(process_after.peak_kib),
        format!("{state_hash:016x}"),
        image_hash
            .map(|h| format!("{h:016x}"))
            .unwrap_or("NA".into()),
        number(clock_pair),
    ]);
    Ok(fields.join(","))
}

fn run(config: Config) -> Result<()> {
    if let Some(mode) = config.child {
        println!("{}", trial(&config, mode)?);
        return Ok(());
    }
    let mut order: Vec<_> = (0..config.trials)
        .flat_map(|trial| config.modes.iter().map(move |&mode| (trial, mode)))
        .collect();
    let mut random = config.shuffle_seed;
    for index in (1..order.len()).rev() {
        random = mix(random.wrapping_add(0x9e37_79b9_7f4a_7c15));
        order.swap(index, random as usize % (index + 1));
    }
    let writer: Box<dyn Write> = if let Some(path) = &config.output {
        Box::new(File::create_new(path)?)
    } else {
        Box::new(std::io::stdout())
    };
    let mut writer = BufWriter::new(writer);
    writeln!(writer, "{HEADER}")?;
    writer.flush()?;
    let mut hashes = HashMap::new();
    for (run_index, (trial_index, mode)) in order.into_iter().enumerate() {
        let at = Instant::now();
        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "--arena-mib",
                &config.mib.to_string(),
                "--workload",
                config.workload.name(),
                "--seed",
                &config.seed.to_string(),
                "--shuffle-seed",
                &config.shuffle_seed.to_string(),
                "--operations",
                &config.operations.to_string(),
                "--checkpoint-at",
                &config.checkpoint_at.to_string(),
                "--internal-trial",
                mode.name(),
                "--trial",
                &trial_index.to_string(),
                "--run-index",
                &run_index.to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        while child.try_wait()?.is_none() {
            if at.elapsed() > Duration::from_secs(120) {
                child.kill()?;
                child.wait()?;
                return Err(format!(
                    "{} trial {trial_index} exceeded 120s; killed and reaped",
                    mode.name()
                )
                .into());
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(format!(
                "{} trial {trial_index} failed: {}; no fallback",
                mode.name(),
                output.status
            )
            .into());
        }
        let row = String::from_utf8(output.stdout)?;
        let row = row.trim();
        let columns: Vec<_> = row.split(',').collect();
        if columns.len() + 1 != HEADER.split(',').count() {
            return Err("child CSV schema mismatch".into());
        }
        let state_index = HEADER
            .split(',')
            .position(|key| key == "state_hash")
            .unwrap();
        let state = columns[state_index].to_owned();
        if let Some(old) = hashes.insert(trial_index, state.clone())
            && old != state
        {
            return Err("cross-mode final hash mismatch".into());
        }
        writeln!(writer, "{row},{}", at.elapsed().as_nanos())?;
        writer.flush()?;
        eprintln!(
            "{} MiB {} trial {trial_index} {} complete",
            config.mib,
            config.workload.name(),
            mode.name()
        );
    }
    Ok(())
}

fn main() {
    if let Err(error) = Config::parse().and_then(run) {
        eprintln!("bench: {error}");
        std::process::exit(1);
    }
}
