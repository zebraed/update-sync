use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, ensure};
use clap::{ArgAction, Parser};
use dunce::canonicalize;
use filetime::{FileTime, set_file_mtime};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(version, about = "One-way sync (source -> target).")]
struct Cli {
    /// Source directory path
    source: PathBuf,
    /// Target directory path
    target: PathBuf,

    /// Disable overwrite when source is newer (default: off)
    #[arg(short = 'w', long = "no-overwrite", action = ArgAction::SetTrue)]
    no_overwrite: bool,

    /// Disable creating directories/files not existing in target (default: off)
    #[arg(short = 'n', long = "no-new", action = ArgAction::SetTrue)]
    no_new: bool,

    /// Disable deleting directories/files not existing in source (default: off)
    #[arg(short = 'd', long = "no-delete", action = ArgAction::SetTrue)]
    no_delete: bool,

    /// Print planned operations without executing
    #[arg(short = 'r', long = "dry-run", alias = "test", default_value_t = false)]
    dry_run: bool,

    /// mtime comparison tolerance in seconds
    #[arg(short = 't', long = "time-tolerance-seconds", default_value_t = 1.0)]
    time_tolerance_seconds: f64,

    /// Path to a .gitignore file; patterns exclude paths from sync (repeat with -i for more)
    #[arg(short = 'i', long = "ignore", value_name = "FILE", action = clap::ArgAction::Append)]
    ignore_files: Vec<PathBuf>,
}

#[derive(Debug)]
struct Snapshot {
    dirs: HashSet<PathBuf>,
    file_mtime: HashMap<PathBuf, SystemTime>,
}

#[derive(Debug, Default)]
struct Plan {
    overwrite_pairs: Vec<PathPair>,
    new_dirs: Vec<PathBuf>,
    new_pairs: Vec<PathPair>,
    delete_files: Vec<PathBuf>,
    delete_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct PathPair {
    src: PathBuf,
    dst: PathBuf,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RunMode {
    DryRun,
    Apply,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let opts = SyncOptions {
        overwrite: !cli.no_overwrite,
        new: !cli.no_new,
        delete: !cli.no_delete,
        dry_run: cli.dry_run,
        tolerance: duration_from_seconds(cli.time_tolerance_seconds)?,
    };
    run_sync(&cli.source, &cli.target, &opts, &cli.ignore_files)
}

#[derive(Debug)]
struct SyncOptions {
    overwrite: bool,
    new: bool,
    delete: bool,
    dry_run: bool,
    tolerance: Duration,
}

fn run_sync(
    source: &Path,
    target: &Path,
    opts: &SyncOptions,
    ignore_files: &[PathBuf],
) -> Result<()> {
    let source = canonicalize(source)
        .with_context(|| format!("failed to resolve source path: {}", source.display()))?;
    let target = resolve_path_allow_missing(target)
        .with_context(|| format!("failed to resolve target path: {}", target.display()))?;

    let src_gitignore = build_gitignore_for_root(&source, ignore_files)?;
    let dst_gitignore = build_gitignore_for_root(&target, ignore_files)?;

    let src = build_snapshot(&source, src_gitignore.as_ref())?;
    let dst = build_snapshot(&target, dst_gitignore.as_ref())?;
    let plan = build_plan(&source, &target, &src, &dst, opts, opts.tolerance);

    if opts.dry_run {
        eprintln!("Dry run: no changes will be written.");
        execute_plan(&plan, opts, RunMode::DryRun)?;
        eprintln!("Done.");
        return Ok(());
    }

    execute_plan(&plan, opts, RunMode::Apply)?;
    eprintln!("Done.");
    Ok(())
}

fn build_gitignore_for_root(
    walk_root: &Path,
    ignore_paths: &[PathBuf],
) -> Result<Option<Gitignore>> {
    if ignore_paths.is_empty() {
        return Ok(None);
    }

    let mut builder = GitignoreBuilder::new(walk_root);
    for p in ignore_paths {
        let resolved = resolve_ignore_path(p)?;
        if let Some(err) = builder.add(&resolved) {
            return Err(err).with_context(|| {
                format!(
                    "failed to read gitignore entries from {}",
                    resolved.display()
                )
            });
        }
    }

    Ok(Some(builder.build()?))
}

fn resolve_ignore_path(ignore_file: &Path) -> Result<PathBuf> {
    let path = if ignore_file.is_absolute() {
        ignore_file.to_path_buf()
    } else {
        std::env::current_dir()
            .with_context(|| "failed to get current directory")?
            .join(ignore_file)
    };

    canonicalize(&path)
        .with_context(|| format!("could not resolve gitignore path {}", path.display()))
}

fn resolve_path_allow_missing(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return canonicalize(path)
            .with_context(|| format!("could not resolve path {}", path.display()));
    }

    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    Ok(std::env::current_dir()
        .with_context(|| "failed to get current directory")?
        .join(path))
}

fn ignored_by_gitignore(gi: Option<&Gitignore>, root: &Path, path: &Path, is_dir: bool) -> bool {
    let Some(gi) = gi else {
        return false;
    };
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };

    gi.matched(rel, is_dir).is_ignore()
}

fn build_snapshot(root: &Path, gitignore: Option<&Gitignore>) -> Result<Snapshot> {
    if !root.exists() {
        return Ok(Snapshot {
            dirs: HashSet::new(),
            file_mtime: HashMap::new(),
        });
    }

    let mut dirs = HashSet::new();
    let mut file_mtime = HashMap::new();

    let walker = WalkDir::new(root).into_iter().filter_entry(|entry| {
        if entry.depth() == 0 {
            return true;
        }
        let is_dir = entry.file_type().is_dir();
        !ignored_by_gitignore(gitignore, root, entry.path(), is_dir)
    });

    for entry in walker {
        let entry = entry.with_context(|| format!("failed to walk {}", root.display()))?;
        let path = entry.path();
        if path == root || ignored_by_gitignore(gitignore, root, path, entry.file_type().is_dir()) {
            continue;
        }
        let rel = relative_path(root, path)?;
        if entry.file_type().is_dir() {
            dirs.insert(rel);
            continue;
        }
        if entry.file_type().is_file() {
            let modified = entry
                .metadata()
                .with_context(|| format!("failed to read metadata {}", path.display()))?
                .modified()
                .with_context(|| format!("failed to read modified time {}", path.display()))?;
            file_mtime.insert(rel, modified);
        }
    }

    Ok(Snapshot { dirs, file_mtime })
}

fn build_plan(
    source: &Path,
    target: &Path,
    src: &Snapshot,
    dst: &Snapshot,
    opts: &SyncOptions,
    tolerance: Duration,
) -> Plan {
    let src_keys: HashSet<_> = src.file_mtime.keys().cloned().collect();
    let dst_keys: HashSet<_> = dst.file_mtime.keys().cloned().collect();

    let same_files: HashSet<_> = src_keys.intersection(&dst_keys).cloned().collect();

    let new_files_rel: HashSet<_> = src_keys.difference(&dst_keys).cloned().collect();
    let del_files_rel: HashSet<_> = dst_keys.difference(&src_keys).cloned().collect();

    let new_dirs_rel: HashSet<_> = src.dirs.difference(&dst.dirs).cloned().collect();
    let del_dirs_rel: HashSet<_> = dst.dirs.difference(&src.dirs).cloned().collect();

    let mut plan = Plan::default();

    if opts.overwrite {
        for rel in same_files {
            let src_m = *src
                .file_mtime
                .get(&rel)
                .expect("same_files should only contain source paths");
            let dst_m = *dst
                .file_mtime
                .get(&rel)
                .expect("same_files should only contain target paths");
            if is_newer_than_with_tolerance(src_m, dst_m, tolerance) {
                plan.overwrite_pairs.push(PathPair {
                    src: source.join(&rel),
                    dst: target.join(&rel),
                });
            }
        }
        plan.overwrite_pairs.sort();
    }

    if opts.new {
        let mut dirs: Vec<_> = new_dirs_rel.into_iter().collect();
        dirs.sort_by(|a, b| path_depth(a).cmp(&path_depth(b)).then_with(|| a.cmp(b)));
        for rel in dirs {
            plan.new_dirs.push(target.join(&rel));
        }
        let mut files: Vec<_> = new_files_rel.into_iter().collect();
        files.sort();
        for rel in files {
            plan.new_pairs.push(PathPair {
                src: source.join(&rel),
                dst: target.join(&rel),
            });
        }
    }

    if opts.delete {
        let mut files: Vec<_> = del_files_rel.into_iter().collect();
        files.sort();
        for rel in files {
            plan.delete_files.push(target.join(&rel));
        }
        let mut dirs: Vec<_> = del_dirs_rel.into_iter().collect();
        dirs.sort_by(|a, b| path_depth(b).cmp(&path_depth(a)).then_with(|| a.cmp(b)));
        for rel in dirs {
            plan.delete_dirs.push(target.join(&rel));
        }
    }

    plan
}

fn execute_plan(plan: &Plan, opts: &SyncOptions, mode: RunMode) -> Result<()> {
    if opts.overwrite {
        for pair in &plan.overwrite_pairs {
            handle_copy_pair(pair, mode, "overwrite (newer)", "overwrote (newer)")?;
        }
    }
    if opts.new {
        for dst in &plan.new_dirs {
            handle_create_directory(dst, mode)?;
        }
        for pair in &plan.new_pairs {
            handle_copy_pair(pair, mode, "copy (new)", "copied (new)")?;
        }
    }
    if opts.delete {
        for target in &plan.delete_files {
            handle_delete_file(target, mode)?;
        }
        for target in &plan.delete_dirs {
            handle_delete_directory(target, mode)?;
        }
    }
    Ok(())
}

fn handle_copy_pair(
    pair: &PathPair,
    mode: RunMode,
    dry_run_label: &str,
    applied_label: &str,
) -> Result<()> {
    match mode {
        RunMode::DryRun => {
            eprintln!(
                "would {}: {} -> {}",
                dry_run_label,
                pair.src.display(),
                pair.dst.display()
            );
            Ok(())
        }
        RunMode::Apply => {
            safe_copy(&pair.src, &pair.dst)?;
            eprintln!(
                "{}: {} -> {}",
                applied_label,
                pair.src.display(),
                pair.dst.display()
            );
            Ok(())
        }
    }
}

fn handle_create_directory(dst: &Path, mode: RunMode) -> Result<()> {
    if dst.exists() {
        return Ok(());
    }

    match mode {
        RunMode::DryRun => {
            eprintln!("would create directory: {}", dst.display());
            Ok(())
        }
        RunMode::Apply => {
            fs::create_dir_all(dst)
                .with_context(|| format!("failed to create directory {}", dst.display()))?;
            eprintln!("created directory: {}", dst.display());
            Ok(())
        }
    }
}

fn handle_delete_file(target: &Path, mode: RunMode) -> Result<()> {
    match mode {
        RunMode::DryRun => {
            eprintln!("would delete file: {}", target.display());
            Ok(())
        }
        RunMode::Apply => match fs::remove_file(target) {
            Ok(_) => {
                eprintln!("deleted file: {}", target.display());
                Ok(())
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("failed to delete file {}", target.display())),
        },
    }
}

fn handle_delete_directory(target: &Path, mode: RunMode) -> Result<()> {
    match mode {
        RunMode::DryRun => {
            eprintln!("would delete directory: {}", target.display());
            Ok(())
        }
        RunMode::Apply => match fs::remove_dir_all(target) {
            Ok(_) => {
                eprintln!("deleted directory: {}", target.display());
                Ok(())
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => {
                Err(e).with_context(|| format!("failed to delete directory {}", target.display()))
            }
        },
    }
}

fn relative_path(root: &Path, path: &Path) -> Result<PathBuf> {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .with_context(|| format!("failed to get relative path for {}", path.display()))
}

fn path_depth(path: &Path) -> usize {
    path.components().count()
}

fn duration_from_seconds(seconds: f64) -> Result<Duration> {
    ensure!(
        seconds.is_finite() && seconds >= 0.0,
        "time tolerance seconds must be a non-negative finite number"
    );
    Ok(Duration::from_secs_f64(seconds))
}

fn is_newer_than_with_tolerance(
    source_mtime: SystemTime,
    target_mtime: SystemTime,
    tolerance: Duration,
) -> bool {
    match source_mtime.duration_since(target_mtime) {
        Ok(delta) => delta > tolerance,
        Err(_) => false,
    }
}

fn safe_copy(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create parent directory {}", parent.display())
            })?;
        }
    }
    fs::copy(src, dst)
        .with_context(|| format!("failed to copy {} -> {}", src.display(), dst.display()))?;

    let src_meta = fs::metadata(src)
        .with_context(|| format!("failed to read source metadata {}", src.display()))?;
    if let Ok(modified) = src_meta.modified() {
        let mtime = FileTime::from_system_time(modified);
        set_file_mtime(dst, mtime)
            .with_context(|| format!("failed to preserve mtime for {}", dst.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn rel_conversion_works_cross_platform() {
        let root = PathBuf::from("base");
        let path = PathBuf::from("base").join("a").join("b.txt");
        let rel = relative_path(&root, &path).expect("relative path");
        assert_eq!(rel, PathBuf::from("a").join("b.txt"));
        assert_eq!(root.join(&rel), path);
    }

    #[test]
    fn newer_than_with_tolerance_respects_threshold() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let newer = base + Duration::from_millis(1500);

        assert!(is_newer_than_with_tolerance(
            newer,
            base,
            Duration::from_secs(1)
        ));
        assert!(!is_newer_than_with_tolerance(
            newer,
            base,
            Duration::from_secs(2)
        ));
        assert!(!is_newer_than_with_tolerance(
            base,
            newer,
            Duration::from_secs(0)
        ));
    }

    #[test]
    fn gitignore_excludes_patterns() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("keep")).expect("create keep");
        fs::create_dir_all(root.join("skip_me")).expect("create ignored dir");
        File::create(root.join("keep").join("a.txt")).expect("create keep file");
        File::create(root.join("skip_me").join("b.txt")).expect("create ignored file");

        let gi_path = root.join(".gitignore");
        let mut f = File::create(&gi_path).expect("gitignore");
        writeln!(f, "skip_me/").expect("write gitignore");

        let gi = build_gitignore_for_root(root, &[gi_path]).expect("build gi");
        let snap = build_snapshot(root, gi.as_ref()).expect("snapshot");
        assert!(
            snap.file_mtime
                .contains_key(&PathBuf::from("keep").join("a.txt"))
        );
        assert!(
            !snap
                .file_mtime
                .contains_key(&PathBuf::from("skip_me").join("b.txt"))
        );
    }

    #[test]
    fn snapshot_without_gitignore_includes_all() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();
        fs::create_dir_all(root.join("any")).expect("create dir");
        File::create(root.join("any").join("x.txt")).expect("create file");
        let snap = build_snapshot(root, None).expect("snapshot");
        assert!(
            snap.file_mtime
                .contains_key(&PathBuf::from("any").join("x.txt"))
        );
    }

    #[test]
    fn snapshot_for_missing_target_is_empty() {
        let tmp = tempdir().expect("tempdir");
        let missing = tmp.path().join("missing-target");
        let snap = build_snapshot(&missing, None).expect("snapshot");
        assert!(snap.dirs.is_empty());
        assert!(snap.file_mtime.is_empty());
    }

    #[test]
    fn run_sync_creates_missing_target_when_copying_new_files() {
        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        fs::create_dir_all(source.join("nested")).expect("create source dir");
        File::create(source.join("nested").join("hello.txt")).expect("create source file");

        let opts = SyncOptions {
            overwrite: true,
            new: true,
            delete: true,
            dry_run: false,
            tolerance: Duration::from_secs(1),
        };

        run_sync(&source, &target, &opts, &[]).expect("sync succeeds");

        assert!(target.join("nested").join("hello.txt").exists());
    }
}
