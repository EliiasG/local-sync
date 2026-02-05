use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Serialize, Deserialize)]
struct FileEntry {
    hash: String,
    synced_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct Manifest {
    files: HashMap<String, FileEntry>,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    match args[1].as_str() {
        "init" => cmd_init(&args)?,
        "push" => cmd_push()?,
        "pull" => cmd_pull()?,
        "status" => cmd_status()?,
        "add" => cmd_add(&args)?,
        "remove" => cmd_remove(&args)?,
        "--help" | "-h" | "help" => print_usage(),
        other => {
            eprintln!("Unknown command: {}", other);
            print_usage();
            std::process::exit(1);
        }
    }

    Ok(())
}

fn print_usage() {
    eprintln!("Usage: local-sync <command>");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  init <path>     Initialize with NAS target path");
    eprintln!("  push            Copy local files to NAS");
    eprintln!("  pull            Copy NAS files to local");
    eprintln!("  status          Show sync status");
    eprintln!("  add <file>      Add a gitignored file to sync");
    eprintln!("  remove <file>   Remove a file from additional sync list");
}

fn cmd_init(args: &[String]) -> Result<()> {
    if args.len() < 3 {
        bail!("Usage: local-sync init <path>");
    }

    let nas_path = PathBuf::from(&args[2]);
    let project_root = std::env::current_dir()?;
    let config_path = project_root.join(".local-sync");

    if config_path.exists() {
        bail!(
            ".local-sync already exists at {}\nRemove it first if you want to reinitialize.",
            config_path.display()
        );
    }

    fs::write(&config_path, format!("{}\n", nas_path.display()))
        .with_context(|| format!("Failed to write {}", config_path.display()))?;

    println!("Initialized local-sync with NAS path: {}", nas_path.display());
    println!("Config written to: {}", config_path.display());

    // Check if NAS already has this project
    let manifest_path = nas_path.join(".local-sync-manifest");
    if manifest_path.exists() {
        println!();
        println!("Project already exists on NAS. Run 'local-sync pull' to download.");
    }

    Ok(())
}

fn cmd_add(args: &[String]) -> Result<()> {
    if args.len() < 3 {
        bail!("Usage: local-sync add <file|directory>");
    }

    let file_path = &args[2];
    let git_root = find_git_root()?;
    let config_path = git_root.join(".local-sync");

    if !config_path.exists() {
        bail!("Not initialized. Run 'local-sync init <path>' first.");
    }

    // Check if file/directory exists
    let full_path = git_root.join(file_path);
    if !full_path.exists() {
        bail!("Path does not exist: {}", file_path);
    }

    // Check if already tracked by git (for files, not directories)
    if full_path.is_file() {
        let git_files = get_git_files(&git_root)?;
        if git_files.contains(&file_path.to_string()) {
            bail!("File is already tracked by git: {}", file_path);
        }
    }

    // Read current config
    let content = fs::read_to_string(&config_path)?;
    let lines: Vec<&str> = content.lines().collect();

    // Check if already added
    for line in &lines[1..] {
        if line.trim() == format!("+{}", file_path) {
            bail!("Path already in sync list: {}", file_path);
        }
    }

    // Append to config
    let mut new_content = content.trim_end().to_string();
    new_content.push_str(&format!("\n+{}\n", file_path));
    fs::write(&config_path, new_content)?;

    let path_type = if full_path.is_dir() { "directory" } else { "file" };
    println!("Added {} to sync: {}", path_type, file_path);
    Ok(())
}

fn cmd_remove(args: &[String]) -> Result<()> {
    if args.len() < 3 {
        bail!("Usage: local-sync remove <file>");
    }

    let file_path = &args[2];
    let git_root = find_git_root()?;
    let config_path = git_root.join(".local-sync");

    if !config_path.exists() {
        bail!("Not initialized. Run 'local-sync init <path>' first.");
    }

    // Check if tracked by git (can't remove git-tracked files)
    let git_files = get_git_files(&git_root)?;
    if git_files.contains(&file_path.to_string()) {
        bail!("Cannot remove git-tracked file from sync: {}", file_path);
    }

    // Read current config
    let content = fs::read_to_string(&config_path)?;
    let lines: Vec<&str> = content.lines().collect();

    let target = format!("+{}", file_path);
    let mut found = false;
    let mut new_lines: Vec<&str> = Vec::new();

    for line in &lines {
        if line.trim() == target {
            found = true;
        } else {
            new_lines.push(line);
        }
    }

    if !found {
        bail!("File not in additional sync list: {}", file_path);
    }

    let new_content = new_lines.join("\n") + "\n";
    fs::write(&config_path, new_content)?;

    println!("Removed from sync: {}", file_path);
    Ok(())
}

fn cmd_push() -> Result<()> {
    let config = get_config()?;
    let sync_files = get_sync_files(&config)?;
    let manifest = load_manifest(&config.nas_path)?;

    // Ensure NAS directory exists
    fs::create_dir_all(&config.nas_path)
        .with_context(|| format!("Failed to create NAS directory: {}", config.nas_path.display()))?;

    let mut new_manifest = Manifest::default();
    let mut conflicts = Vec::new();
    let mut to_copy = Vec::new();
    let mut to_delete = Vec::new();

    // Check each file to sync
    for rel_path in &sync_files {
        let local_path = config.git_root.join(rel_path);
        let nas_file_path = config.nas_path.join(rel_path);

        if !local_path.exists() {
            continue;
        }

        let local_hash = hash_file(&local_path)?;

        // Check for conflicts
        if let Some(manifest_entry) = manifest.files.get(rel_path) {
            if nas_file_path.exists() {
                let nas_hash = hash_file(&nas_file_path)?;
                // Conflict: both changed since last sync
                if local_hash != manifest_entry.hash && nas_hash != manifest_entry.hash {
                    conflicts.push(rel_path.clone());
                    continue;
                }
            }
        }

        // Check if copy needed
        let needs_copy = if nas_file_path.exists() {
            let nas_hash = hash_file(&nas_file_path)?;
            local_hash != nas_hash
        } else {
            true
        };

        if needs_copy {
            to_copy.push((rel_path.clone(), local_path.clone(), nas_file_path.clone()));
        }

        new_manifest.files.insert(
            rel_path.clone(),
            FileEntry {
                hash: local_hash,
                synced_at: chrono::Utc::now(),
            },
        );
    }

    // Find deleted files (in manifest but not in sync files)
    let sync_files_set: HashSet<_> = sync_files.iter().cloned().collect();
    for (rel_path, _) in &manifest.files {
        if !sync_files_set.contains(rel_path) {
            let nas_file_path = config.nas_path.join(rel_path);
            if nas_file_path.exists() {
                to_delete.push((rel_path.clone(), nas_file_path));
            }
        }
    }

    // Handle conflicts
    if !conflicts.is_empty() {
        eprintln!("Conflicts detected (modified both locally and on NAS):");
        for path in &conflicts {
            eprintln!("  {}", path);
        }
        eprintln!();
        if !prompt_continue("Do you want to continue? Local changes will overwrite NAS.")? {
            eprintln!("Aborted.");
            return Ok(());
        }

        // Re-add conflicts to copy list
        for rel_path in conflicts {
            let local_path = config.git_root.join(&rel_path);
            let nas_file_path = config.nas_path.join(&rel_path);
            let local_hash = hash_file(&local_path)?;

            to_copy.push((rel_path.clone(), local_path, nas_file_path));
            new_manifest.files.insert(
                rel_path,
                FileEntry {
                    hash: local_hash,
                    synced_at: chrono::Utc::now(),
                },
            );
        }
    }

    // Perform copies
    for (rel_path, local_path, nas_file_path) in &to_copy {
        if let Some(parent) = nas_file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(local_path, nas_file_path)
            .with_context(|| format!("Failed to copy {}", rel_path))?;
        println!("Copied: {}", rel_path);
    }

    // Perform deletions
    for (rel_path, nas_file_path) in &to_delete {
        fs::remove_file(nas_file_path)
            .with_context(|| format!("Failed to delete {}", rel_path))?;
        println!("Deleted: {}", rel_path);
        // Clean up empty parent directories
        cleanup_empty_dirs(&config.nas_path, nas_file_path)?;
    }

    // Save manifest
    save_manifest(&config.nas_path, &new_manifest)?;

    let total_changes = to_copy.len() + to_delete.len();
    if total_changes == 0 {
        println!("Already up to date.");
    } else {
        println!(
            "Push complete: {} copied, {} deleted",
            to_copy.len(),
            to_delete.len()
        );
    }

    Ok(())
}

fn cmd_pull() -> Result<()> {
    let config = get_config_for_pull()?;
    let manifest = load_manifest(&config.nas_path)?;

    if manifest.files.is_empty() && !config.nas_path.exists() {
        println!("Nothing to pull. NAS folder is empty or doesn't exist.");
        return Ok(());
    }

    let mut new_manifest = Manifest::default();
    let mut conflicts = Vec::new();
    let mut to_copy = Vec::new();
    let mut to_delete = Vec::new();

    // Check each file in manifest
    for (rel_path, manifest_entry) in &manifest.files {
        let local_path = config.git_root.join(rel_path);
        let nas_file_path = config.nas_path.join(rel_path);

        if !nas_file_path.exists() {
            // File deleted on NAS, delete locally if it exists
            if local_path.exists() {
                to_delete.push((rel_path.clone(), local_path.clone()));
            }
            continue;
        }

        let nas_hash = hash_file(&nas_file_path)?;

        // Check for conflicts
        if local_path.exists() {
            let local_hash = hash_file(&local_path)?;
            // Conflict: both changed since last sync
            if local_hash != manifest_entry.hash && nas_hash != manifest_entry.hash {
                conflicts.push(rel_path.clone());
                new_manifest.files.insert(
                    rel_path.clone(),
                    FileEntry {
                        hash: nas_hash,
                        synced_at: chrono::Utc::now(),
                    },
                );
                continue;
            }
        }

        // Check if copy needed
        let needs_copy = if local_path.exists() {
            let local_hash = hash_file(&local_path)?;
            local_hash != nas_hash
        } else {
            true
        };

        if needs_copy {
            to_copy.push((rel_path.clone(), nas_file_path.clone(), local_path.clone()));
        }

        new_manifest.files.insert(
            rel_path.clone(),
            FileEntry {
                hash: nas_hash,
                synced_at: chrono::Utc::now(),
            },
        );
    }

    // Also check for new files on NAS that aren't in manifest
    // (could happen if manifest was lost or this is first pull)
    if config.nas_path.exists() {
        for entry in walkdir(&config.nas_path)? {
            let rel_path = entry
                .strip_prefix(&config.nas_path)
                .unwrap()
                .to_string_lossy()
                .to_string();

            if rel_path == ".local-sync-manifest" {
                continue;
            }

            if !manifest.files.contains_key(&rel_path) {
                let local_path = config.git_root.join(&rel_path);
                let nas_file_path = config.nas_path.join(&rel_path);

                if !local_path.exists() {
                    to_copy.push((rel_path.clone(), nas_file_path.clone(), local_path));
                    let nas_hash = hash_file(&nas_file_path)?;
                    new_manifest.files.insert(
                        rel_path,
                        FileEntry {
                            hash: nas_hash,
                            synced_at: chrono::Utc::now(),
                        },
                    );
                }
            }
        }
    }

    // Handle conflicts
    if !conflicts.is_empty() {
        eprintln!("Conflicts detected (modified both locally and on NAS):");
        for path in &conflicts {
            eprintln!("  {}", path);
        }
        eprintln!();
        if !prompt_continue("Do you want to continue? NAS changes will overwrite local.")? {
            eprintln!("Aborted.");
            return Ok(());
        }

        // Re-add conflicts to copy list
        for rel_path in conflicts {
            let local_path = config.git_root.join(&rel_path);
            let nas_file_path = config.nas_path.join(&rel_path);
            to_copy.push((rel_path, nas_file_path, local_path));
        }
    }

    // Perform copies
    for (rel_path, nas_file_path, local_path) in &to_copy {
        if let Some(parent) = local_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(nas_file_path, local_path)
            .with_context(|| format!("Failed to copy {}", rel_path))?;
        println!("Copied: {}", rel_path);
    }

    // Perform deletions
    for (rel_path, local_path) in &to_delete {
        fs::remove_file(local_path)
            .with_context(|| format!("Failed to delete {}", rel_path))?;
        println!("Deleted: {}", rel_path);
    }

    // Save manifest
    save_manifest(&config.nas_path, &new_manifest)?;

    let total_changes = to_copy.len() + to_delete.len();
    if total_changes == 0 {
        println!("Already up to date.");
    } else {
        println!(
            "Pull complete: {} copied, {} deleted",
            to_copy.len(),
            to_delete.len()
        );
    }

    Ok(())
}

fn cmd_status() -> Result<()> {
    let config = get_config()?;
    let sync_files = get_sync_files(&config)?;
    let manifest = load_manifest(&config.nas_path)?;

    println!("Git root: {}", config.git_root.display());
    println!("NAS path: {}", config.nas_path.display());
    println!("Synced files: {}", sync_files.len());
    println!("Additional files: {}", config.additional_files.len());
    println!("Manifest entries: {}", manifest.files.len());

    let mut local_only = 0;
    let mut nas_only = 0;
    let mut modified = 0;
    let mut in_sync = 0;

    let sync_files_set: HashSet<_> = sync_files.iter().cloned().collect();

    for rel_path in &sync_files {
        let local_path = config.git_root.join(rel_path);
        let nas_file_path = config.nas_path.join(rel_path);

        if !local_path.exists() {
            continue;
        }

        if !nas_file_path.exists() {
            local_only += 1;
        } else {
            let local_hash = hash_file(&local_path)?;
            let nas_hash = hash_file(&nas_file_path)?;
            if local_hash == nas_hash {
                in_sync += 1;
            } else {
                modified += 1;
            }
        }
    }

    for (rel_path, _) in &manifest.files {
        if !sync_files_set.contains(rel_path) {
            nas_only += 1;
        }
    }

    println!();
    println!("Status:");
    println!("  In sync: {}", in_sync);
    println!("  Modified: {}", modified);
    println!("  Local only: {}", local_only);
    println!("  NAS only: {}", nas_only);

    if !config.additional_files.is_empty() {
        println!();
        println!("Additional files:");
        for file in &config.additional_files {
            println!("  +{}", file);
        }
    }

    Ok(())
}

struct Config {
    git_root: PathBuf,
    nas_path: PathBuf,
    additional_files: Vec<String>,
}

fn get_config() -> Result<Config> {
    let git_root = find_git_root()?;
    load_config_from_root(git_root)
}

fn get_config_for_pull() -> Result<Config> {
    let project_root = find_project_root()?;
    load_config_from_root(project_root)
}

fn load_config_from_root(root: PathBuf) -> Result<Config> {
    let config_path = root.join(".local-sync");

    if !config_path.exists() {
        bail!(
            "No .local-sync config file found in: {}\n\
             Create a .local-sync file containing the NAS target path.",
            root.display()
        );
    }

    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;

    let mut lines = content.lines();
    let nas_path_str = lines
        .next()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!(".local-sync file is empty. It should contain the NAS target path."))?;

    let nas_path = PathBuf::from(nas_path_str);

    let additional_files: Vec<String> = lines
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('+') {
                Some(trimmed[1..].to_string())
            } else {
                None
            }
        })
        .collect();

    Ok(Config {
        git_root: root,
        nas_path,
        additional_files,
    })
}

fn get_sync_files(config: &Config) -> Result<Vec<String>> {
    let mut files = get_git_files(&config.git_root)?;
    let mut files_set: HashSet<_> = files.iter().cloned().collect();

    // Always include git config files if they exist
    for git_file in &[".gitignore", ".gitattributes"] {
        if config.git_root.join(git_file).exists() && !files_set.contains(*git_file) {
            files_set.insert(git_file.to_string());
            files.push(git_file.to_string());
        }
    }

    // Always include .git directory if it exists
    let git_dir = config.git_root.join(".git");
    if git_dir.exists() && git_dir.is_dir() {
        for file_path in walkdir(&git_dir)? {
            if let Ok(rel_path) = file_path.strip_prefix(&config.git_root) {
                let rel_str = rel_path.to_string_lossy().to_string();
                if !files_set.contains(&rel_str) {
                    files_set.insert(rel_str.clone());
                    files.push(rel_str);
                }
            }
        }
    }

    // Add additional files/directories that aren't already in git
    for entry in &config.additional_files {
        let full_path = config.git_root.join(entry);

        if full_path.is_dir() {
            // Expand directory to all files within
            for file_path in walkdir(&full_path)? {
                if let Ok(rel_path) = file_path.strip_prefix(&config.git_root) {
                    let rel_str = rel_path.to_string_lossy().to_string();
                    if !files_set.contains(&rel_str) {
                        files_set.insert(rel_str.clone());
                        files.push(rel_str);
                    }
                }
            }
        } else if !files_set.contains(entry) {
            files_set.insert(entry.clone());
            files.push(entry.clone());
        }
    }

    Ok(files)
}

fn find_git_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("Failed to run git")?;

    if !output.status.success() {
        bail!("Not in a git repository");
    }

    let path = String::from_utf8(output.stdout)
        .context("Invalid UTF-8 in git output")?
        .trim()
        .to_string();

    Ok(PathBuf::from(path))
}

fn find_project_root() -> Result<PathBuf> {
    let mut current = std::env::current_dir()?;
    loop {
        if current.join(".local-sync").exists() {
            return Ok(current);
        }
        if !current.pop() {
            bail!("No .local-sync file found in current directory or any parent directory");
        }
    }
}

fn get_git_files(git_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(git_root)
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .output()
        .context("Failed to run git ls-files")?;

    if !output.status.success() {
        bail!("git ls-files failed");
    }

    let files: Vec<String> = String::from_utf8(output.stdout)
        .context("Invalid UTF-8 in git output")?
        .lines()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    Ok(files)
}

fn hash_file(path: &Path) -> Result<String> {
    let content = fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&content);
    let result = hasher.finalize();
    Ok(format!("sha256:{:x}", result))
}

fn load_manifest(nas_path: &Path) -> Result<Manifest> {
    let manifest_path = nas_path.join(".local-sync-manifest");
    if !manifest_path.exists() {
        return Ok(Manifest::default());
    }

    let content = fs::read_to_string(&manifest_path)
        .with_context(|| format!("Failed to read manifest: {}", manifest_path.display()))?;

    let manifest: Manifest =
        serde_json::from_str(&content).with_context(|| "Failed to parse manifest")?;

    Ok(manifest)
}

fn save_manifest(nas_path: &Path, manifest: &Manifest) -> Result<()> {
    let manifest_path = nas_path.join(".local-sync-manifest");
    let content = serde_json::to_string_pretty(manifest).context("Failed to serialize manifest")?;
    fs::write(&manifest_path, content)
        .with_context(|| format!("Failed to write manifest: {}", manifest_path.display()))?;
    Ok(())
}

fn prompt_continue(message: &str) -> Result<bool> {
    eprint!("{} [Y/n] ", message);
    io::stderr().flush()?;

    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;

    let response = line.trim().to_lowercase();
    Ok(response.is_empty() || response == "y" || response == "yes")
}

fn cleanup_empty_dirs(root: &Path, file_path: &Path) -> Result<()> {
    let mut current = file_path.parent();
    while let Some(dir) = current {
        if dir == root {
            break;
        }
        if dir.read_dir()?.next().is_none() {
            fs::remove_dir(dir)?;
        } else {
            break;
        }
        current = dir.parent();
    }
    Ok(())
}

fn walkdir(path: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walkdir_recursive(path, &mut files)?;
    Ok(files)
}

fn walkdir_recursive(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        files.push(path.to_path_buf());
        return Ok(());
    }

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        if entry_path.is_dir() {
            walkdir_recursive(&entry_path, files)?;
        } else {
            files.push(entry_path);
        }
    }

    Ok(())
}
