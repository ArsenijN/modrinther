use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::io::{self};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use console::Term;
use zip::ZipArchive;

#[derive(Debug, Deserialize, Serialize)]
struct ModrinthIndex {
    dependencies: HashMap<String, String>,
    files: Vec<ModFile>,
    #[serde(rename = "formatVersion")]
    format_version: u32,
    game: String,
    name: String,
    #[serde(rename = "versionId")]
    version_id: String,
    #[serde(skip)]
    overrides_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ModFile {
    downloads: Vec<String>,
    env: HashMap<String, String>,
    #[serde(rename = "fileSize")]
    file_size: u64,
    hashes: HashMap<String, String>,
    path: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Clear the console
    let term = Term::stdout();
    let _ = term.clear_screen();
    
    // Get the path to the file either from arguments or via drag-and-drop
    let args: Vec<String> = std::env::args().collect();
    let input_path = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        println!("Drag and drop a Modrinth .json, .zip, or .mrpack file onto this executable,");
        println!("or provide it as an argument: modrinther <path-to-file>");
        
        // Wait for user input (so the console doesn't close immediately)
        println!("\nPress Enter to exit...");
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        std::process::exit(1);
    };

    let (index, base_dir) = if is_archive_file(&input_path) {
        // Handle ZIP or MRPACK file
        println!("Processing archive file: {}", input_path.display());
        process_archive_file(&input_path)?
    } else {
        // Handle JSON file directly
        let index_content = fs::read_to_string(&input_path).map_err(|e| {
            format!("Failed to read index file '{}': {}", input_path.display(), e)
        })?;
        
        let mut index: ModrinthIndex = serde_json::from_str(&index_content).map_err(|e| {
            format!("Failed to parse JSON: {}", e)
        })?;

        // Check if "overrides" folder exists
        let overrides_path = input_path.parent()
                                .unwrap_or(Path::new("."))
                                .join("overrides");
                                
        if overrides_path.exists() && overrides_path.is_dir() {
            println!("Found overrides directory at: {}", overrides_path.display());
            index.overrides_path = Some(overrides_path);
        }
        
        (index, input_path.parent().unwrap_or(Path::new(".")).to_path_buf())
    };

    // Use the modpack name as the output directory name
    let pack_name = sanitize_filename(&index.name);
    let output_dir = base_dir.join(&pack_name);

    // Create output directory if it doesn't exist
    fs::create_dir_all(&output_dir)?;

    println!("Installing modpack: {}", index.name);
    println!("Output directory: {}", output_dir.display());
    
    // Clone dependencies to avoid borrowing issues
    let deps = index.dependencies.clone();
    println!("Minecraft version: {}", deps.get("minecraft").unwrap_or(&"unknown".to_string()));
    
    // Detect loader type and version
    let loader_type = if deps.contains_key("fabric-loader") {
        "Fabric"
    } else if deps.contains_key("forge") {
        "Forge"
    } else if deps.contains_key("quilt-loader") {
        "Quilt"
    } else {
        "Unknown"
    };
    
    // Create a separate string for the loader version to avoid temporary value issues
    let unknown_str = "unknown".to_string();
    let loader_version = match loader_type {
        "Fabric" => deps.get("fabric-loader"),
        "Forge" => deps.get("forge"),
        "Quilt" => deps.get("quilt-loader"),
        _ => None
    }.unwrap_or(&unknown_str);
    
    println!("Loader: {} {}", loader_type, loader_version);
    println!("Total files to download: {}", index.files.len());

    // Copy overrides if they exist
    if let Some(overrides_path) = &index.overrides_path {
        println!("Copying overrides...");
        copy_directory_contents(overrides_path, &output_dir)?;
    }

    // Setup progress bars
    let mp = MultiProgress::new();
    let main_pb = mp.add(ProgressBar::new(index.files.len() as u64));
    main_pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} files ({percent}%) - {elapsed_precise}")
        .unwrap()
        .progress_chars("#>-"));

    let download_pb = mp.add(ProgressBar::new(1));
    download_pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({percent}%) {wide_msg}")
        .unwrap()
        .progress_chars("#>-"));
    
    // Track the current downloading file name
    let current_file = Arc::new(Mutex::new(String::new()));
    let download_pb = Arc::new(download_pb);

    // Create a summary for later use
    let mut summary = String::new();
    summary.push_str(&format!("Modpack: {}\n", index.name));
    summary.push_str(&format!("Minecraft version: {}\n", deps.get("minecraft").unwrap_or(&unknown_str)));
    summary.push_str(&format!("Loader: {} {}\n", loader_type, loader_version));
    summary.push_str(&format!("Total mods: {}\n\n", index.files.len()));
    summary.push_str("Installed mods:\n");
    
    for file in &index.files {
        let file_name = Path::new(&file.path).file_name()
            .unwrap_or_default()
            .to_string_lossy();
        summary.push_str(&format!("- {} ({} bytes)\n", file_name, file.file_size));
    }

    // Process files in parallel
    let index_arc = Arc::new(index);
    let output_dir_arc = Arc::new(output_dir);
    let main_pb_arc = Arc::new(main_pb);

    // Use a maximum of 5 concurrent downloads
    let results = stream::iter(0..index_arc.files.len())
        .map(|i| {
            let index = Arc::clone(&index_arc);
            let output_dir = Arc::clone(&output_dir_arc);
            let main_pb = Arc::clone(&main_pb_arc);
            let download_pb = Arc::clone(&download_pb);
            let current_file = Arc::clone(&current_file);
            
            async move {
                let file = &index.files[i];
                let file_name = Path::new(&file.path).file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                
                {
                    let mut current = current_file.lock().unwrap();
                    *current = file_name.clone();
                }
                
                download_pb.set_length(file.file_size);
                download_pb.set_position(0);
                download_pb.set_message(format!("Downloading {}", file_name));
                
                let result = download_file(&file, &output_dir, &download_pb).await;
                
                if result.is_ok() {
                    download_pb.finish_with_message(format!("Downloaded {}", file_name));
                } else if let Err(e) = &result {
                    download_pb.finish_with_message(format!("Failed to download {}: {}", file_name, e));
                }
                
                main_pb.inc(1);
                result
            }
        })
        .buffer_unordered(5) // Max 5 concurrent downloads
        .collect::<Vec<_>>()
        .await;

    main_pb_arc.finish_with_message("Downloads completed!");

    // Count success and failures
    let success_count = results.iter().filter(|r| r.is_ok()).count();
    let error_count = results.iter().filter(|r| r.is_err()).count();
    
    println!("\nInstallation complete!");
    println!("Successfully downloaded: {}/{}", success_count, index_arc.files.len());
    
    // Create a summary file
    let summary_path = output_dir_arc.join("modpack_summary.txt");
    fs::write(&summary_path, summary)?;
    
    if error_count > 0 {
        println!("Failed to download: {}/{}", error_count, index_arc.files.len());
        println!("Errors:");
        for (i, result) in results.iter().enumerate() {
            if let Err(e) = result {
                println!("  - {}: {}", index_arc.files[i].path, e);
            }
        }
    }

    println!("Created summary file at: {}", summary_path.display());

    // Prevent the window from closing immediately
    println!("\nPress Enter to exit...");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(())
}

// Helper function to check if a file is an archive (ZIP or MRPACK)
fn is_archive_file(path: &Path) -> bool {
    if let Some(ext) = path.extension() {
        let ext_str = ext.to_string_lossy().to_lowercase();
        return ext_str == "zip" || ext_str == "mrpack";
    }
    false
}

fn process_archive_file(archive_path: &Path) -> Result<(ModrinthIndex, PathBuf), Box<dyn Error>> {
    // Create temp directory for extraction
    let temp_dir = std::env::temp_dir().join("modrinth_temp");
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)?;
    }
    fs::create_dir_all(&temp_dir)?;
    
    println!("Extracting archive to temporary directory: {}", temp_dir.display());
    
    // Open the archive file (ZIP or MRPACK)
    let file = fs::File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    
    // Extract all files
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let outpath = temp_dir.join(file.name());
        
        if file.name().ends_with('/') {
            fs::create_dir_all(&outpath)?;
        } else {
            if let Some(p) = outpath.parent() {
                if !p.exists() {
                    fs::create_dir_all(p)?;
                }
            }
            let mut outfile = fs::File::create(&outpath)?;
            io::copy(&mut file, &mut outfile)?;
        }
    }
    
    // Find the modrinth.index.json file
    let index_path = find_index_json(&temp_dir)?;
    
    // Read and parse the index file
    let index_content = fs::read_to_string(&index_path).map_err(|e| {
        format!("Failed to read index file '{}': {}", index_path.display(), e)
    })?;
    
    let mut index: ModrinthIndex = serde_json::from_str(&index_content).map_err(|e| {
        format!("Failed to parse JSON: {}", e)
    })?;
    
    // Check if "overrides" folder exists in the extracted archive
    let overrides_path = index_path.parent()
                           .unwrap_or(Path::new("."))
                           .join("overrides");
                           
    if overrides_path.exists() && overrides_path.is_dir() {
        println!("Found overrides directory at: {}", overrides_path.display());
        index.overrides_path = Some(overrides_path);
    }
    
    Ok((index, archive_path.parent().unwrap_or(Path::new(".")).to_path_buf()))
}

fn find_index_json(dir: &Path) -> Result<PathBuf, Box<dyn Error>> {
    // First check if modrinth.index.json exists in the root
    let index_path = dir.join("modrinth.index.json");
    if index_path.exists() {
        return Ok(index_path);
    }
    
    // Otherwise, search recursively
    fn search_recursive(dir: &Path) -> Option<PathBuf> {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.file_name()?.to_string_lossy() == "modrinth.index.json" {
                    return Some(path);
                } else if path.is_dir() {
                    if let Some(found) = search_recursive(&path) {
                        return Some(found);
                    }
                }
            }
        }
        None
    }
    
    if let Some(path) = search_recursive(dir) {
        Ok(path)
    } else {
        Err("Could not find modrinth.index.json in the archive file".into())
    }
}

fn sanitize_filename(name: &str) -> String {
    let invalid_chars = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let mut sanitized = name.to_string();
    
    for c in invalid_chars {
        sanitized = sanitized.replace(c, "_");
    }
    
    sanitized
}

async fn download_file(
    file: &ModFile, 
    output_dir: &Path, 
    progress_bar: &ProgressBar
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let url = &file.downloads[0]; // Use the first download URL
    let file_path = output_dir.join(&file.path);
    
    // Create parent directories if they don't exist
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    
    // Create client and download
    let client = reqwest::Client::new();
    let response = client.get(url).send().await?;
    
    if !response.status().is_success() {
        return Err(format!("Failed to download: HTTP {}", response.status()).into());
    }
    
    // Create file and write data
    let mut file = File::create(&file_path).await?;
    let mut stream = response.bytes_stream();
    
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        progress_bar.inc(chunk.len() as u64);
    }
    
    file.flush().await?;
    
    Ok(())
}

fn copy_directory_contents(src: &Path, dst: &Path) -> Result<(), Box<dyn Error>> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_directory_contents(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}