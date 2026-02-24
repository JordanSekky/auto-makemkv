use anyhow::Result;
use std::path::PathBuf;

pub fn fmt_bytes(bytes: usize) -> String {
    const UNITS: &[&str] = &["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = UNITS[0];
    for &u in &UNITS[1..] {
        if value < 1000.0 {
            break;
        }
        value /= 1000.0;
        unit = u;
    }
    if unit == "B" {
        format!("{} B", bytes)
    } else {
        format!("{:.2} {}", value, unit)
    }
}

pub fn get_incremented_dir(root: &PathBuf, base_name: &str) -> Result<PathBuf> {
    // Pattern: NN_BaseName
    // Find max NN
    let mut max_n = 0;

    if root.exists() {
        for entry in std::fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    // Check if name matches NN_BaseName exactly
                    if let Some((num_str, rest)) = name.split_once('_') {
                        if rest == base_name {
                            if let Ok(num) = num_str.parse::<u32>() {
                                if num > max_n {
                                    max_n = num;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let next_n = max_n + 1;
    let folder_name = format!("{:02}_{}", next_n, base_name);
    Ok(root.join(folder_name))
}
