use std::{
    collections::BTreeMap,
    env,
    path::Path,
    sync::{OnceLock, RwLock},
};

#[cfg(test)]
use std::sync::Mutex;

fn overrides() -> &'static RwLock<BTreeMap<String, String>> {
    static OVERRIDES: OnceLock<RwLock<BTreeMap<String, String>>> = OnceLock::new();
    OVERRIDES.get_or_init(|| RwLock::new(BTreeMap::new()))
}

pub fn get(key: &str) -> Option<String> {
    env::var(key).ok().or_else(|| {
        overrides()
            .read()
            .expect("runtime env overrides poisoned")
            .get(key)
            .cloned()
    })
}

pub fn load_dotenv_for_workflow(workflow_path: &Path) -> Result<(), String> {
    let mut merged = BTreeMap::new();
    if let Some(directory) = workflow_path.parent() {
        merge_dotenv_files_in_dir(directory, &mut merged)?;
    }

    let mut overlay = overrides().write().expect("runtime env overrides poisoned");
    overlay.clear();
    overlay.extend(merged);
    Ok(())
}

fn merge_dotenv_files_in_dir(
    directory: &Path,
    merged: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let mut directory_values = BTreeMap::new();

    for filename in [".env", ".env.local"] {
        let path = directory.join(filename);
        if !path.is_file() {
            continue;
        }

        let iter = dotenvy::from_path_iter(&path)
            .map_err(|error| format!("dotenv_error path={} reason={error}", path.display()))?;
        for entry in iter {
            let (key, value) = entry
                .map_err(|error| format!("dotenv_error path={} reason={error}", path.display()))?;
            directory_values.insert(key, value);
        }
    }

    for (key, value) in directory_values {
        if env::var_os(&key).is_none() {
            merged.entry(key).or_insert(value);
        }
    }

    Ok(())
}

#[cfg(test)]
pub fn clear_for_tests() {
    overrides()
        .write()
        .expect("runtime env overrides poisoned")
        .clear();
}

#[cfg(test)]
pub fn test_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}
