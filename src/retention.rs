use std::{path::Path, time::SystemTime};

use tokio::fs;

pub async fn cleanup_retained_files<F>(
    dir: &Path,
    retention_count: usize,
    matches: F,
) -> anyhow::Result<usize>
where
    F: Fn(&Path) -> bool,
{
    if retention_count == 0 {
        return Ok(0);
    }

    let mut files = Vec::new();
    let mut read_dir = match fs::read_dir(dir).await {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err.into()),
    };

    while let Some(entry) = read_dir.next_entry().await? {
        let path = entry.path();
        if !entry.file_type().await?.is_file() || !matches(&path) {
            continue;
        }

        let modified = entry
            .metadata()
            .await?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((modified, path));
    }

    if files.len() <= retention_count {
        return Ok(0);
    }

    files.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.to_string_lossy().cmp(&b.1.to_string_lossy()))
    });

    let remove_count = files.len() - retention_count;
    let mut removed = 0usize;
    for (_, path) in files.into_iter().take(remove_count) {
        match fs::remove_file(&path).await {
            Ok(()) => removed += 1,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }

    Ok(removed)
}
